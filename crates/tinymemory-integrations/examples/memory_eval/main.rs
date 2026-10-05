//! An accuracy and latency eval of the agent memory lifecycle.
//!
//! A scripted agent (`agent`) plays twelve scenarios (`scenarios`) through
//! the real lifecycle calls: brain lookups, a restart, contradicting facts, a
//! tool-heavy incident, a team handoff, compaction, tenant isolation, a
//! needle in noise, explicit learnings, learning from corrections, a
//! surprise, and conflicting sources. After each scenario's writes settle,
//! its probes are scored (`score`). Every scenario then runs a belief build
//! over its whole tree and is probed again, so the effect of synthesis shows
//! up as a second phase. The run ends with its KPIs (`kpi`): accuracy,
//! learning, surprise, conflicts, cost and latency.
//!
//! ```sh
//! # Offline, against the reference engine:
//! cargo run -p tinymemory-integrations --features full --example memory_eval
//!
//! # Against a CortexDB (see integration/cortexdb/ and docs/evals/):
//! CORTEX_DB_URL=http://127.0.0.1:3142 CORTEX_DB_KEY=tinymemory-cortex-test \
//!   cargo run -p tinymemory-integrations --features full --example memory_eval -- \
//!   --label mock --json target/memory-eval/mock.json
//! ```
//!
//! Flags:
//!
//! - `--engine reference|cortex`: the default is `cortex` when
//!   `CORTEX_DB_URL` is set, and `reference` otherwise.
//! - `--only <scenario>[,<scenario>…]`: run only these scenarios.
//! - `--enrich-wait <secs>`: the longest to wait for CortexDB's enrichment
//!   queue (fact extraction) to drain before the belief build (default 600
//!   against CortexDB, 0 otherwise). With a real model it takes minutes.
//! - `--json <path>`: write every probe, pack included, as JSON.
//! - `--label <name>`: name the run in the report.
//! - `--llm`: also have a model answer every probe from its pack (see
//!   `llm`).
//!
//! `memory_eval compare <run.json>…` compares the reports of several runs
//! instead (see `compare`). A run records the CortexDB flag profile it ran
//! under when `CORTEX_FLAGS_FILE` names one (see
//! `integration/cortexdb/flags/` and `scripts/memory-flag-sweep.sh`).
//!
//! Everything is written below roots unique to the run and forgotten at the
//! end, unless `CORTEX_DB_KEEP` is set.

mod agent;
mod compare;
mod inspect;
mod kpi;
mod llm;
mod scenarios;
mod score;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use serde::Serialize;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    ConsolidateRequest, ForgetTarget, ListRequest, MemoryEngine, MemoryMeta, Reach, Role,
    StoreItem, Turn,
};
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};
use tinymemory_tools::context::{self, Brief, ContextSpec};
use tinymemory_tools::{
    AgentMemory, BackgroundJob, Brain, BrainDocument, Compaction, ContextPack, JobOutcome,
    MemoryLayout, PreTurn, RecallPolicy, SessionStart,
};

use agent::{ScriptedAgent, ms};
use inspect::{Captured, Derived, Inspector, Usage};
use llm::Llm;
use scenarios::{MAIN, Probe, Scenario, Step, Via};
use score::{Latency, ProbeResult, Totals, grade, score};

type Error = Box<dyn std::error::Error>;

/// Turns of a thread the scripted agent keeps in its prompt.
const WINDOW: u32 = 8;

/// The longest a scenario's writes may take to become visible.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Consecutive empty, quiet polls of the enrichment queue (3 s apart) that
/// count as drained.
const DRAINED_POLLS: u32 = 4;

/// The command line.
struct Args {
    engine: String,
    only: Option<String>,
    enrich_wait: Option<u64>,
    json: Option<String>,
    label: String,
    llm: bool,
}

fn args() -> Result<Args, Error> {
    let mut parsed = Args {
        engine: if std::env::var("CORTEX_DB_URL").is_ok() {
            "cortex".into()
        } else {
            "reference".into()
        },
        only: None,
        enrich_wait: None,
        json: None,
        label: String::new(),
        llm: false,
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--engine" => parsed.engine = value()?,
            "--only" => parsed.only = Some(value()?),
            "--enrich-wait" => parsed.enrich_wait = Some(value()?.parse()?),
            "--json" => parsed.json = Some(value()?),
            "--label" => parsed.label = value()?,
            "--llm" => parsed.llm = true,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    if parsed.label.is_empty() {
        parsed.label = parsed.engine.clone();
    }
    Ok(parsed)
}

/// Latency samples by step.
#[derive(Default, Serialize)]
struct Timings(BTreeMap<String, Vec<f64>>);

impl Timings {
    fn add(&mut self, step: &str, ms: f64) {
        self.0.entry(step.to_string()).or_default().push(ms);
    }
}

/// What the synthesis step did for a scenario.
#[derive(Debug, Default, Serialize)]
struct Synthesis {
    jobs: usize,
    outcomes: BTreeMap<String, usize>,
    scopes: usize,
    /// Beliefs the builds reported building.
    built: usize,
    ms: f64,
    derived: Vec<Derived>,
    /// Every fact, belief and conflict CortexDB holds for the scenario.
    captured: Captured,
}

/// One scenario's results.
#[derive(Serialize)]
struct ScenarioReport {
    name: &'static str,
    about: &'static str,
    writes: usize,
    tool_calls: usize,
    settle_ms: f64,
    synthesis: Synthesis,
    probes: Vec<ProbeResult>,
    /// What CortexDB's models spent on this scenario.
    usage: Option<Usage>,
}

/// The CortexDB flag profile a run is under: its name, and every flag it
/// sets, the baseline's included.
fn profile() -> Result<(Option<String>, BTreeMap<String, String>), Error> {
    let Ok(file) = std::env::var("CORTEX_FLAGS_FILE") else {
        return Ok((None, BTreeMap::new()));
    };
    let file = std::path::Path::new(&file);
    let mut flags = BTreeMap::new();
    for path in [file.with_file_name("baseline.env"), file.to_path_buf()] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().map(str::trim) {
            if let Some((key, value)) = line.split_once('=').filter(|_| !line.starts_with('#')) {
                flags.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    let name = file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or("CORTEX_FLAGS_FILE names no file")?;
    Ok((Some(name.to_string()), flags))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("compare") {
        return compare::run(&raw[1..]);
    }
    let args = args()?;
    let (profile, flags) = profile()?;
    let url = std::env::var("CORTEX_DB_URL").unwrap_or_default();
    let key = std::env::var("CORTEX_DB_KEY").unwrap_or_else(|_| "tinymemory-cortex-test".into());
    let (engine, inspector): (Arc<dyn MemoryEngine>, Option<Inspector>) = match args.engine.as_str()
    {
        "reference" => (Arc::new(ReferenceEngine::new()), None),
        "cortex" if !url.is_empty() => (
            Arc::new(CortexEngine::direct(&url, CortexCredential::api_key(&key))?),
            Some(Inspector::new(&url, &key)),
        ),
        "cortex" => return Err("--engine cortex needs CORTEX_DB_URL".into()),
        other => return Err(format!("unknown engine {other}").into()),
    };
    let enrich_wait = args
        .enrich_wait
        .unwrap_or(if inspector.is_some() { 600 } else { 0 });
    let llm = if args.llm {
        Some(Llm::from_env()?)
    } else {
        None
    };
    let run = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    println!(
        "memory eval `{}`: engine {} ({:?}), answer model {}, run {run}\n",
        args.label,
        engine.descriptor().id,
        engine.health().await,
        llm.as_ref().map_or("none", |llm| llm.model.as_str()),
    );

    let eval = Eval {
        engine: engine.clone(),
        inspector,
        llm,
        run,
        enrich_wait,
        policy: RecallPolicy {
            build_beliefs_every: Some(4),
            ..RecallPolicy::default()
        },
    };
    let usage_before = match &eval.inspector {
        Some(inspector) => Some(inspector.usage().await?),
        None => None,
    };
    let mut timings = Timings::default();
    let mut reports = Vec::new();
    for scenario in scenarios::all() {
        if args
            .only
            .as_deref()
            .is_some_and(|only| !only.split(',').any(|name| name == scenario.name))
        {
            continue;
        }
        println!("== {}: {}", scenario.name, scenario.about);
        let report = eval.scenario(&scenario, &mut timings).await?;
        for phase in ["recall", "synthesis"] {
            let totals = Totals::of(report.probes.iter().filter(|p| p.phase == phase));
            println!(
                "   {phase:<9} hits {:<12} answers {:<12} MRR {:.2}",
                Totals::pct(totals.hits, totals.scored),
                Totals::pct(totals.answers_ok, totals.scored),
                totals.mrr,
            );
        }
        reports.push(report);
    }

    print_summary(&args.label, &reports, &timings);
    let usage = match (&eval.inspector, usage_before) {
        (Some(inspector), Some(before)) => Some(inspector.usage().await?.since(&before)),
        _ => None,
    };
    if let Some(usage) = &usage {
        println!(
            "\n## Model usage\n\nCortexDB's models: {} calls, {} tokens, ${:.2} as its routers \
             price them (excludes the `--llm` answers).\n",
            usage.calls, usage.tokens, usage.cost_usd
        );
        println!("| Scenario | Calls | Tokens | Cost |");
        println!("| --- | --- | --- | --- |");
        for report in &reports {
            if let Some(spent) = &report.usage {
                println!(
                    "| {} | {} | {} | ${:.3} |",
                    report.name, spent.calls, spent.tokens, spent.cost_usd
                );
            }
        }
    }
    let kpis = kpi::compute(&reports, usage.as_ref(), &timings);
    kpi::print(&args.label, &kpis);
    let server = match &eval.inspector {
        Some(inspector) => inspector.version().await.ok(),
        None => None,
    };
    if let Some(path) = &args.json {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = serde_json::json!({
            "label": args.label,
            "profile": profile,
            "flags": flags,
            "server": server,
            "engine": engine.descriptor().id,
            "run": run,
            "scenarios": reports,
            "timings": timings,
            "usage": usage,
            "kpis": kpis,
        });
        std::fs::write(path, serde_json::to_string_pretty(&out)?)?;
        println!("\nwrote {path}");
    }
    Ok(())
}

/// The layout of `tenant` in `scenario` for this run.
fn layout(run: u64, scenario: &str, tenant: &str) -> Result<MemoryLayout, Error> {
    let root = format!("project:eval-{run}-{}-{tenant}", scenario.replace('_', "-"));
    Ok(MemoryLayout::new(root.parse()?)?)
}

/// The tenants a scenario touches.
fn tenants(scenario: &Scenario) -> Vec<&'static str> {
    let mut tenants: Vec<&'static str> = scenario
        .steps
        .iter()
        .map(|step| match step {
            Step::Doc { tenant, .. } | Step::Chat { tenant, .. } => *tenant,
            Step::Learning { .. } => MAIN,
        })
        .chain(scenario.probes.iter().map(|probe| probe.tenant))
        .collect();
    tenants.sort_unstable();
    tenants.dedup();
    tenants
}

/// One eval run: the engine, the optional helpers, and the settings every
/// scenario shares.
struct Eval {
    engine: Arc<dyn MemoryEngine>,
    inspector: Option<Inspector>,
    llm: Option<Llm>,
    run: u64,
    enrich_wait: u64,
    policy: RecallPolicy,
}

impl Eval {
    /// Writes `scenario`, probes it, synthesises, and probes it again.
    async fn scenario(
        &self,
        scenario: &Scenario,
        timings: &mut Timings,
    ) -> Result<ScenarioReport, Error> {
        let (engine, run, policy) = (&self.engine, self.run, &self.policy);
        let usage_before = match &self.inspector {
            Some(inspector) => Some(inspector.usage().await?),
            None => None,
        };
        let memory = |tenant: &str, agent: &str| -> Result<AgentMemory, Error> {
            Ok(
                AgentMemory::new(engine.clone(), layout(run, scenario.name, tenant)?, agent)?
                    .with_policy(policy.clone()),
            )
        };
        let epoch = Utc
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .single()
            .ok_or("a valid epoch")?;

        // Writes.
        let mut jobs: Vec<BackgroundJob> = Vec::new();
        let mut writes: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut tool_calls = 0;
        for step in &scenario.steps {
            match step {
                Step::Doc {
                    tenant,
                    source,
                    title,
                    text,
                } => {
                    let brain = Brain::new(engine.clone(), layout(run, scenario.name, tenant)?);
                    let started = Instant::now();
                    let ingested = brain
                        .ingest(BrainDocument::new(source.clone(), *text).titled(*title))
                        .await?;
                    timings.add("brain ingest (visible)", ms(started));
                    jobs.push(ingested.job);
                    *writes.entry(tenant).or_default() += 1;
                }
                Step::Learning {
                    kind,
                    text,
                    confidence,
                } => {
                    let layout = layout(run, scenario.name, MAIN)?;
                    let meta = MemoryMeta {
                        namespace: layout.learnings().clone(),
                        ..MemoryMeta::default()
                    };
                    engine
                        .store(StoreItem::learning(*text, *kind, *confidence, meta))
                        .await?;
                    *writes.entry(MAIN).or_default() += 1;
                }
                Step::Chat {
                    tenant,
                    agent,
                    thread,
                    day,
                    turns,
                } => {
                    let mut scripted = ScriptedAgent::new(memory(tenant, agent)?, thread, WINDOW)
                        .at(epoch + chrono::Duration::days(*day));
                    for (text, tools) in turns {
                        let record = scripted.user(text, tools).await?;
                        timings.add("pre_turn (log + recall)", record.pre_ms);
                        timings.add("post_turn (log)", record.post_ms);
                        if !record.logged {
                            println!("   ! a turn of {thread} was not logged");
                        }
                        tool_calls += record.tool_calls;
                        jobs.extend(record.jobs);
                        *writes.entry(tenant).or_default() += 2;
                    }
                }
            }
        }

        // Settle: wait until every write is listed.
        let started = Instant::now();
        for (tenant, expected) in &writes {
            self.settle(&layout(run, scenario.name, tenant)?, *expected)
                .await?;
        }
        let settle_ms = ms(started);
        timings.add("settle (all writes listed)", settle_ms);

        let mut probes = Vec::new();
        for probe in &scenario.probes {
            probes.push(self.probe(scenario, probe, "recall", timings).await?);
        }

        // Synthesis: the jobs the writes handed back, then one build per tenant
        // over its whole tree.
        if let Some(inspector) = &self.inspector {
            let started = Instant::now();
            let cap = Duration::from_secs(self.enrich_wait);
            // Drained means empty with nothing newly queued for a few polls
            // in a row: the queue empties between batches while later
            // writes are still being taken in.
            let mut quiet = 0;
            let mut last_queued = None;
            while quiet < DRAINED_POLLS && started.elapsed() < cap {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let (pending, queued) = inspector.enrichment().await?;
                let settled = pending == 0 && last_queued == Some(queued);
                quiet = if settled { quiet + 1 } else { 0 };
                last_queued = Some(queued);
            }
            let waited = ms(started);
            timings.add("enrichment (queue drained)", waited);
            println!("   enrichment drained in {:.0} s", waited / 1e3);
        }
        for tenant in tenants(scenario) {
            let root = layout(run, scenario.name, tenant)?.root().clone();
            jobs.push(BackgroundJob::BuildBeliefs {
                request: ConsolidateRequest::new(Reach::subtree(root)),
            });
        }
        let runner = memory(MAIN, "eval")?.background();
        let mut synthesis = Synthesis {
            jobs: jobs.len(),
            ..Synthesis::default()
        };
        let started = Instant::now();
        for job in jobs {
            let report = runner.run(job).await?;
            let outcome = match &report.outcome {
                JobOutcome::Done => "done",
                JobOutcome::Started => "started",
                JobOutcome::Scheduled => "scheduled",
                JobOutcome::Skipped { .. } => "skipped",
            };
            *synthesis.outcomes.entry(outcome.to_string()).or_default() += 1;
            if let Some(receipt) = report.consolidation {
                synthesis.scopes += receipt.scopes;
                synthesis.built += receipt.built.unwrap_or_default();
            }
        }
        synthesis.ms = ms(started);
        timings.add("synthesis (all builds)", synthesis.ms);
        let questions: Vec<&str> = scenario.probes.iter().map(|p| p.question).collect();
        let questions = questions.join(" ");
        if let Some(inspector) = &self.inspector {
            for tenant in tenants(scenario) {
                let layout = layout(run, scenario.name, tenant)?;
                let node = layout.root().to_string();
                let scopes = inspector.scopes(&node).await?;
                for scope in &scopes {
                    synthesis
                        .derived
                        .push(inspector.derived(scope, &questions).await?);
                }
                synthesis
                    .captured
                    .extend(inspector.captured(&scopes).await?);
            }
        }
        let beliefs: usize = synthesis.derived.iter().map(|d| d.beliefs).sum();
        let facts: usize = synthesis.derived.iter().map(|d| d.facts).sum();
        println!(
            "   synthesis {:?} over {} scopes in {:.0} ms: {} beliefs built; \
             recall finds {facts} facts, {beliefs} beliefs",
            synthesis.outcomes, synthesis.scopes, synthesis.ms, synthesis.built
        );
        if self.inspector.is_some() {
            let held = &synthesis.captured;
            println!(
                "   captured {} facts, {} beliefs, {} conflicts",
                held.facts.len(),
                held.beliefs.len(),
                held.conflicts.len()
            );
            for conflict in &held.conflicts {
                println!("     conflict {conflict}");
            }
        }

        for probe in &scenario.probes {
            let mut result = self.probe(scenario, probe, "synthesis", timings).await?;
            if self.inspector.is_some() && !probe.expect.is_empty() {
                let held = &synthesis.captured;
                result.captured = Some(
                    probe.expect.iter().all(|needle| held.mentions(needle))
                        || probe.accept.iter().any(|needle| held.mentions(needle)),
                );
            }
            probes.push(result);
        }

        if std::env::var("CORTEX_DB_KEEP").is_err() {
            for tenant in tenants(scenario) {
                engine
                    .forget(ForgetTarget::Filter(
                        layout(run, scenario.name, tenant)?.holistic_filter(),
                    ))
                    .await?;
            }
        }
        let usage = match (&self.inspector, usage_before) {
            (Some(inspector), Some(before)) => Some(inspector.usage().await?.since(&before)),
            _ => None,
        };
        Ok(ScenarioReport {
            usage,
            name: scenario.name,
            about: scenario.about,
            writes: writes.values().sum(),
            tool_calls,
            settle_ms,
            synthesis,
            probes,
        })
    }

    /// Waits until `layout` lists at least `expected` items.
    async fn settle(&self, layout: &MemoryLayout, expected: usize) -> Result<(), Error> {
        let started = Instant::now();
        loop {
            let mut listed = 0;
            let mut cursor = None;
            loop {
                let mut req = ListRequest::new(layout.holistic_filter(), 100);
                req.cursor = cursor;
                let page = self.engine.list(req).await?;
                listed += page.items.len();
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            if listed >= expected {
                return Ok(());
            }
            if started.elapsed() > SETTLE_TIMEOUT {
                return Err(format!(
                    "only {listed} of {expected} writes visible under {}",
                    layout.root()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Reads `probe` the way it says and scores the pack.
    async fn probe(
        &self,
        scenario: &Scenario,
        probe: &Probe,
        phase: &'static str,
        timings: &mut Timings,
    ) -> Result<ProbeResult, Error> {
        let (engine, run, policy, llm) = (&self.engine, self.run, &self.policy, self.llm.as_ref());
        let memory = AgentMemory::new(
            engine.clone(),
            layout(run, scenario.name, probe.tenant)?,
            probe.agent,
        )?
        .with_policy(policy.clone());
        let started = Instant::now();
        let (markdown, tokens) = match &probe.via {
            Via::Ask => {
                let thread = format!("probe-{}", probe.id);
                pack(
                    memory
                        .pre_turn(PreTurn::new(thread, 0, probe.question))
                        .await?
                        .pack,
                )
            }
            Via::Resume { thread, focus } => pack(
                memory
                    .start_session(SessionStart {
                        thread_id: thread.map(str::to_owned),
                        focus: focus.map(str::to_owned),
                    })
                    .await?,
            ),
            Via::Compact { thread, dropped } => pack(
                memory
                    .recall_for_compaction(Compaction {
                        thread_id: (*thread).to_string(),
                        dropped: dropped
                            .iter()
                            .map(|text| Turn::new(Role::User, text.as_str()))
                            .collect(),
                        focus: None,
                    })
                    .await?,
            ),
            Via::Continue {
                thread,
                turn_index,
                in_prompt_from,
            } => {
                let mut pre = PreTurn::new(*thread, *turn_index, probe.question);
                pre.in_prompt_from = *in_prompt_from;
                pack(memory.pre_turn(pre).await?.pack)
            }
            Via::ContextDoc { heading } => {
                let root = layout(run, scenario.name, probe.tenant)?.root().clone();
                let spec = ContextSpec {
                    briefs: vec![Brief::new(*heading, probe.question)],
                    reach: Some(Reach::subtree(root)),
                    ..ContextSpec::default()
                };
                let doc = context::compile(engine.as_ref(), &spec).await?;
                (doc.markdown, doc.tokens)
            }
        };
        let elapsed = ms(started);
        let mut result = score(scenario.name, phase, probe, &markdown, tokens, elapsed);
        if let Some(llm) = llm {
            let answer = llm.answer(&markdown, probe.question).await?;
            result.llm_ok = grade(probe, Some(&answer.text));
            result.llm_answer = Some(answer.text);
            result.llm_tokens = answer.tokens;
            result.llm_cost_usd = answer.cost_usd;
        }
        timings.add(&format!("probe {}", result.via), elapsed);
        Ok(result)
    }
}

/// A pack's markdown and token count.
fn pack(pack: ContextPack) -> (String, usize) {
    (pack.markdown, pack.tokens)
}

/// One accuracy row.
fn row(name: &str, phase: &str, t: &Totals) -> String {
    format!(
        "| {name} | {phase} | {} | {} | {} | {} | {:.2} | {} | {} |",
        Totals::pct(t.hits, t.scored),
        Totals::pct(t.answers_ok, t.scored),
        Totals::pct(t.llm_ok, t.llm_scored),
        Totals::pct(t.captured, t.captured_checked),
        t.mrr,
        Totals::pct(t.fresh_first, t.contradictions),
        if t.leak_checks == 0 {
            "–".to_string()
        } else {
            format!("{}/{}", t.leaks, t.leak_checks)
        },
    )
}

fn print_summary(label: &str, reports: &[ScenarioReport], timings: &Timings) {
    let phases = ["recall", "synthesis"];
    println!("\n## Accuracy (`{label}`)\n");
    println!(
        "| Scenario | Phase | Pack hit | Extractive answer | Model answer | Captured | MRR | Fresh first | Leaks |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let all: Vec<&ProbeResult> = reports.iter().flat_map(|r| &r.probes).collect();
    for report in reports {
        for phase in phases {
            let t = Totals::of(report.probes.iter().filter(|p| p.phase == phase));
            println!("{}", row(report.name, phase, &t));
        }
    }
    for phase in phases {
        let t = Totals::of(all.iter().copied().filter(|p| p.phase == phase));
        println!("{}", row("**all**", phase, &t));
    }

    println!("\n## By question style\n");
    println!(
        "| Style | Phase | Pack hit | Extractive answer | Model answer | Captured | MRR | Fresh first | Leaks |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for style in ["lexical", "paraphrase"] {
        for phase in phases {
            let t = Totals::of(
                all.iter()
                    .copied()
                    .filter(|p| p.phase == phase && p.style == style),
            );
            println!("{}", row(style, phase, &t));
        }
    }

    println!("\n## Latency (ms)\n");
    println!("| Step | n | p50 | p95 | max |");
    println!("| --- | --- | --- | --- | --- |");
    for (step, samples) in &timings.0 {
        let l = Latency::of(samples);
        println!(
            "| {step} | {} | {:.1} | {:.1} | {:.1} |",
            l.n, l.p50, l.p95, l.max
        );
    }

    println!("\n## Misses\n");
    for result in all.iter().filter(|p| {
        p.hit == Some(false)
            || p.answer_ok == Some(false)
            || p.llm_ok == Some(false)
            || p.leak
            || p.stale_first
    }) {
        println!(
            "- {} {}/{} ({}, {}): hit {:?}, rank {:?}, stale first {}, leak {}; extractive {:?}; model {:?}",
            result.phase,
            result.scenario,
            result.id,
            result.via,
            result.style,
            result.hit,
            result.rank,
            result.stale_first,
            result.leak,
            result
                .answer
                .as_deref()
                .map(|a| a.chars().take(90).collect::<String>()),
            result.llm_answer,
        );
    }
}
