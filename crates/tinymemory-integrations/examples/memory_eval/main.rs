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
//!
//! # Against hosted memory, behind the TinyHumans backend (billed to the
//! # account the token belongs to; use a test account):
//! TINYHUMANS_API_URL=https://api.tinyhumans.ai TINYHUMANS_API_KEY=tiny_live_… \
//!   cargo run -p tinymemory-integrations --features full --example memory_eval -- \
//!   --engine tinyhumans --llm --json target/memory-eval/hosted.json
//! ```
//!
//! Flags:
//!
//! - `--engine reference|cortex|tinyhumans`: the default is `cortex` when
//!   `CORTEX_DB_URL` is set, and `reference` otherwise. `tinyhumans` reads
//!   `TINYHUMANS_API_URL` (default: the production backend) and
//!   `TINYHUMANS_API_KEY` (a session JWT or `tiny_live_` key). Hosted memory
//!   exposes no admin routes, so the run has no enrichment queue to poll, no
//!   usage accounting and no derived-layer inspection.
//! - `--only <scenario>[,<scenario>…]`: run only these scenarios.
//! - `--enrich-wait <secs>`: the longest to wait for CortexDB's enrichment
//!   queue (fact extraction) to drain before the belief build (default 600
//!   against CortexDB, 0 otherwise). With a real model it takes minutes.
//!   On `tinyhumans`, where the queue cannot be read, it is a plain wait
//!   (default 60: production derives facts 30–45 s after a write).
//! - `--json <path>`: write every probe, pack included, as JSON.
//! - `--label <name>`: name the run in the report.
//! - `--llm`: also have a model answer every probe from its pack (see
//!   `llm`).
//! - `--host openhuman`: run scripted turns through OpenHuman's dated
//!   pre-turn hook with its 5000 ms deadline, policy and logged tool results.
//! - `--layout v3`: use CortexDB's per-person scope tree and pooled chats.
//! - `--team-limit <n>`: override the host's team-conversation budget.
//! - `--brain-limit <n>`: override the number of ranked brain documents in a
//!   pack, for accuracy-versus-context-budget experiments.
//! - `--loop-guard`: replay a JSON-scripted 500-turn thread and inspect all
//!   stored items for an injected `<memory-context>` tag.
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
mod instrument;
mod kpi;
mod llm;
mod loop_guard;
mod safety;
mod scenarios;
mod score;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use serde::Serialize;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    ConsolidateRequest, FetchMode, FetchRequest, ForgetTarget, ListRequest, MAX_STORE_MANY,
    MemoryEngine, MemoryMeta, Reach, Role, StoreItem, Turn, TurnRange, WriteOptions,
};
use tinymemory_integrations::cortex::{
    CortexCredential, CortexEngine, StaticBearer, TINYHUMANS_API_ENDPOINT,
};
use tinymemory_tools::context::{self, Brief, ContextSpec};
use tinymemory_tools::{
    AgentMemory, BackgroundJob, Brain, BrainDocument, Compaction, ContextPack, JobOutcome,
    MemoryLayout, PreTurn, RecallPolicy, SessionStart,
};

use agent::{HostHook, PRE_TURN_TIMEOUT, ScriptedAgent, ms};
use inspect::{Captured, Derived, Inspector, Usage};
use llm::Llm;
use scenarios::{MAIN, Probe, Scenario, Step, Via};
use score::{Latency, ProbeResult, Totals, grade, score};

/// The plain enrichment wait on hosted memory, where the queue cannot be
/// read: production derives facts 30–45 s after a write.
const HOSTED_ENRICH_WAIT_SECS: u64 = 60;

type Error = Box<dyn std::error::Error>;

/// Turns of a thread the scripted agent keeps in its prompt.
const WINDOW: u32 = 8;

/// The longest a scenario's writes may take to become visible.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Bulk accepted writes can take longer to become fully listable.
const SCALE_SETTLE_TIMEOUT: Duration = Duration::from_secs(300);

fn settle_timeout(scenario: &str) -> Duration {
    if scenario == "needle_scale" {
        SCALE_SETTLE_TIMEOUT
    } else {
        SETTLE_TIMEOUT
    }
}

fn settle_page_size(expected: usize) -> usize {
    expected.clamp(100, 1_000)
}

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
    host: String,
    layout: String,
    team_limit: Option<usize>,
    brain_limit: Option<usize>,
    date_hint: bool,
    scale_events: Option<usize>,
    scale_position: String,
    ranked_wait: u64,
    seed: u64,
    safety_audit: bool,
    expect_derived: bool,
    loop_guard: bool,
}

fn args() -> Result<Args, Error> {
    parse_args(std::env::args().skip(1))
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args, Error> {
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
        host: "scripted".into(),
        layout: "legacy".into(),
        team_limit: None,
        brain_limit: None,
        date_hint: false,
        scale_events: None,
        scale_position: "middle".into(),
        ranked_wait: 0,
        seed: 251,
        safety_audit: false,
        expect_derived: false,
        loop_guard: false,
    };
    let mut raw = raw.into_iter();
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--engine" => parsed.engine = value()?,
            "--only" => parsed.only = Some(value()?),
            "--enrich-wait" => parsed.enrich_wait = Some(value()?.parse()?),
            "--json" => parsed.json = Some(value()?),
            "--label" => parsed.label = value()?,
            "--llm" => parsed.llm = true,
            "--host" => parsed.host = value()?,
            "--layout" => parsed.layout = value()?,
            "--team-limit" => parsed.team_limit = Some(value()?.parse()?),
            "--brain-limit" => parsed.brain_limit = Some(value()?.parse()?),
            "--date-hint" => parsed.date_hint = true,
            "--scale-events" => parsed.scale_events = Some(value()?.parse()?),
            "--scale-position" => parsed.scale_position = value()?,
            "--ranked-wait" => parsed.ranked_wait = value()?.parse()?,
            "--seed" => parsed.seed = value()?.parse()?,
            "--safety-audit" => parsed.safety_audit = true,
            "--expect-derived" => parsed.expect_derived = true,
            "--loop-guard" => parsed.loop_guard = true,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    if !matches!(parsed.host.as_str(), "scripted" | "openhuman") {
        return Err(format!("unknown host {}", parsed.host).into());
    }
    if !matches!(parsed.layout.as_str(), "legacy" | "v3") {
        return Err(format!("unknown layout {}", parsed.layout).into());
    }
    if parsed.loop_guard && parsed.host != "openhuman" {
        return Err("--loop-guard requires --host openhuman".into());
    }
    if parsed.date_hint && parsed.host != "openhuman" {
        return Err("--date-hint requires --host openhuman".into());
    }
    if parsed.expect_derived && !parsed.safety_audit {
        return Err("--expect-derived requires --safety-audit".into());
    }
    if parsed.ranked_wait > 0 && parsed.scale_events.is_none() {
        return Err("--ranked-wait requires --scale-events".into());
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
    /// Whether the queue reached four quiet polls before the wait cap.
    enrichment_drained: Option<bool>,
}

/// One scenario's results.
#[derive(Serialize)]
struct ScenarioReport {
    name: &'static str,
    about: &'static str,
    writes: usize,
    tool_calls: usize,
    pre_turn_timeouts: usize,
    settle_ms: f64,
    synthesis: Synthesis,
    probes: Vec<ProbeResult>,
    /// What CortexDB's models spent on this scenario.
    usage: Option<Usage>,
    ranked_ready: Option<bool>,
    ranked_wait_ms: Option<f64>,
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
    instrument::install();
    let (profile, flags) = profile()?;
    let models: BTreeMap<&str, String> = [
        "CORTEX_EMBEDDING_MODEL",
        "CORTEX_EXTRACTION_MODEL",
        "CORTEX_ENRICHMENT_MODEL",
        "CORTEX_ANSWER_MODEL",
        "CORTEX_VERIFIER_MODEL",
    ]
    .into_iter()
    .filter_map(|name| std::env::var(name).ok().map(|value| (name, value)))
    .collect();
    let url = std::env::var("CORTEX_DB_URL").unwrap_or_default();
    let key = std::env::var("CORTEX_DB_KEY").unwrap_or_else(|_| "tinymemory-cortex-test".into());
    let (engine, inspector): (Arc<dyn MemoryEngine>, Option<Inspector>) = match args.engine.as_str()
    {
        "reference" => (Arc::new(ReferenceEngine::new()), None),
        "cortex" if !url.is_empty() => {
            let engine = CortexEngine::direct(&url, CortexCredential::api_key(&key))?;
            let engine = if args.layout == "v3" {
                engine.with_scope_root(&format!("org:eval-{}", std::process::id()), None)?
            } else {
                engine
            };
            (Arc::new(engine), Some(Inspector::new(&url, &key)))
        }
        "cortex" => return Err("--engine cortex needs CORTEX_DB_URL".into()),
        "tinyhumans" => {
            let base = std::env::var("TINYHUMANS_API_URL")
                .unwrap_or_else(|_| TINYHUMANS_API_ENDPOINT.to_string());
            let token = std::env::var("TINYHUMANS_API_KEY")
                .map_err(|_| "--engine tinyhumans needs TINYHUMANS_API_KEY")?;
            let engine = CortexEngine::tinyhumans(&base, Arc::new(StaticBearer::new(token)))?;
            let engine = if args.layout == "v3" {
                engine.with_tenant_root()?
            } else {
                engine
            };
            (Arc::new(engine), None)
        }
        other => return Err(format!("unknown engine {other}").into()),
    };
    let hosted = args.engine == "tinyhumans";
    let enrich_wait = args.enrich_wait.unwrap_or(if inspector.is_some() {
        600
    } else if hosted {
        HOSTED_ENRICH_WAIT_SECS
    } else {
        0
    });
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

    let mut policy = if args.host == "openhuman" {
        RecallPolicy {
            team_limit: 0,
            ..RecallPolicy::default()
        }
    } else {
        RecallPolicy {
            build_beliefs_every: Some(4),
            ..RecallPolicy::default()
        }
    };
    if let Some(team_limit) = args.team_limit {
        policy.team_limit = team_limit;
    }
    if let Some(brain_limit) = args.brain_limit {
        policy.brain_limit = brain_limit;
    }
    let eval = Eval {
        engine: engine.clone(),
        inspector,
        hosted,
        llm,
        run,
        enrich_wait,
        policy,
        openhuman: args.host == "openhuman",
        pooled: args.layout == "v3",
        date_hint: args.date_hint,
        ranked_wait: args.ranked_wait,
    };
    let usage_before = match &eval.inspector {
        Some(inspector) => Some(inspector.usage().await?),
        None => None,
    };
    let mut timings = Timings::default();
    let mut reports = Vec::new();
    let cases = if let Some(count) = args.scale_events {
        vec![scenarios::scaled(count, &args.scale_position, args.seed)?]
    } else {
        scenarios::all()
    };
    for scenario in cases {
        if scenario.name == "long_compaction" && args.only.is_none() {
            continue;
        }
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

    let loop_guard = if args.loop_guard {
        let report = loop_guard::run(engine.clone(), run, &eval.policy, eval.pooled).await?;
        println!(
            "\n## Loop guard\n\n```json\n{}\n```",
            serde_json::to_string_pretty(&report)?
        );
        Some(report)
    } else {
        None
    };
    let safety = if args.safety_audit {
        let report = safety::run(
            engine.clone(),
            eval.inspector.as_ref(),
            run,
            args.expect_derived,
            eval.pooled,
        )
        .await?;
        println!(
            "\n## Safety audit\n\n```json\n{}\n```",
            serde_json::to_string_pretty(&report)?
        );
        Some(report)
    } else {
        None
    };
    instrument::drain(&mut timings);
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
            "models": models,
            "probe_answer_model": eval.llm.as_ref().map(|llm| llm.model.as_str()),
            "probe_answer_prompt": eval.llm.as_ref().map(|_| llm::PROMPT_VERSION),
            "server": server,
            "engine": engine.descriptor().id,
            "host": args.host,
            "layout": args.layout,
            "team_limit": eval.policy.team_limit,
            "brain_limit": eval.policy.brain_limit,
            "pre_turn_timeout_ms": PRE_TURN_TIMEOUT.as_millis() as u64,
            "date_hint": args.date_hint,
            "scale_events": args.scale_events,
            "scale_position": args.scale_position,
            "ranked_wait": args.ranked_wait,
            "seed": args.seed,
            "run": run,
            "scenarios": reports,
            "loop_guard": loop_guard,
            "safety": safety,
            "expect_derived": args.expect_derived,
            "timings": timings,
            "usage": usage,
            "kpis": kpis,
        });
        std::fs::write(path, serde_json::to_string_pretty(&out)?)?;
        println!("\nwrote {path}");
    }
    if loop_guard
        .as_ref()
        .is_some_and(|report| report.echoed_items > 0)
    {
        return Err("loop guard found injected pack text in stored items".into());
    }
    if safety
        .as_ref()
        .is_some_and(|report| report.violations() > 0)
    {
        return Err("safety audit found a leaked or residual item".into());
    }
    Ok(())
}

/// The layout of `tenant` in `scenario` for this run.
fn layout(run: u64, scenario: &str, tenant: &str, pooled: bool) -> Result<MemoryLayout, Error> {
    let root = format!("project:eval-{run}-{}-{tenant}", scenario.replace('_', "-"));
    let layout = MemoryLayout::new(root.parse()?)?;
    if pooled {
        Ok(layout.with_pooled_conversations(&"ws:main".parse()?)?)
    } else {
        Ok(layout)
    }
}

/// The tenants a scenario touches.
fn tenants(scenario: &Scenario) -> Vec<&'static str> {
    let mut tenants: Vec<&'static str> = scenario
        .steps
        .iter()
        .map(|step| match step {
            Step::BulkDocuments { .. } => MAIN,
            Step::Doc { tenant, .. } | Step::Chat { tenant, .. } => *tenant,
            Step::Learning { .. } => MAIN,
        })
        .chain(scenario.probes.iter().map(|probe| probe.tenant))
        .collect();
    tenants.sort_unstable();
    tenants.dedup();
    tenants
}

/// The host queue need only execute one pending belief build per identical
/// scope: each build reads all accepted writes when it runs. Keep ingestion
/// jobs in order because they carry distinct documents.
fn coalesce_builds(jobs: Vec<BackgroundJob>) -> Vec<BackgroundJob> {
    let mut builds = Vec::new();
    let mut ready = Vec::new();
    for job in jobs {
        if matches!(job, BackgroundJob::BuildBeliefs { .. }) {
            if builds.contains(&job) {
                continue;
            }
            builds.push(job.clone());
        }
        ready.push(job);
    }
    ready
}

/// One eval run: the engine, the optional helpers, and the settings every
/// scenario shares.
struct Eval {
    engine: Arc<dyn MemoryEngine>,
    inspector: Option<Inspector>,
    /// The engine is hosted memory: enrichment cannot be watched, so the
    /// run waits `enrich_wait` instead. No other engine waits blind.
    hosted: bool,
    llm: Option<Llm>,
    run: u64,
    enrich_wait: u64,
    policy: RecallPolicy,
    openhuman: bool,
    pooled: bool,
    date_hint: bool,
    ranked_wait: u64,
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
            Ok(AgentMemory::new(
                engine.clone(),
                layout(run, scenario.name, tenant, self.pooled)?,
                agent,
            )?
            .with_policy(policy.clone()))
        };
        let epoch = Utc
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .single()
            .ok_or("a valid epoch")?;

        // Writes.
        let mut jobs: Vec<BackgroundJob> = Vec::new();
        let mut writes: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut tool_calls = 0;
        let mut pre_turn_timeouts = 0;
        for step in &scenario.steps {
            match step {
                Step::BulkDocuments {
                    count,
                    needle_at,
                    seed,
                } => {
                    let node = layout(run, scenario.name, MAIN, self.pooled)?
                        .brain(&tinymemory_tools::BrainSource::Files)?;
                    let started = Instant::now();
                    for start in (0..*count).step_by(MAX_STORE_MANY) {
                        let batch = (start..(*count).min(start + MAX_STORE_MANY))
                            .map(|index| {
                                StoreItem::document(
                                    scenarios::scale_document(index, *needle_at, *seed),
                                    MemoryMeta {
                                        namespace: node.clone(),
                                        ..MemoryMeta::default()
                                    },
                                )
                            })
                            .collect();
                        engine
                            .store_many_with(batch, WriteOptions::accepted())
                            .await?;
                    }
                    timings.add("bulk ingest (accepted)", ms(started));
                    *writes.entry(MAIN).or_default() += *count;
                }
                Step::Doc {
                    tenant,
                    source,
                    title,
                    text,
                } => {
                    let brain = Brain::new(
                        engine.clone(),
                        layout(run, scenario.name, tenant, self.pooled)?,
                    );
                    let started = Instant::now();
                    let ingested = brain
                        .ingest(BrainDocument::new(source.clone(), *text).titled(*title))
                        .await?;
                    timings.add("brain ingest (visible)", ms(started));
                    jobs.extend(ingested.job);
                    *writes.entry(tenant).or_default() += 1;
                }
                Step::Learning {
                    kind,
                    text,
                    confidence,
                } => {
                    let layout = layout(run, scenario.name, MAIN, self.pooled)?;
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
                    if self.openhuman {
                        scripted = scripted.openhuman();
                        if self.date_hint {
                            scripted = scripted.date_hint();
                        }
                    }
                    for (text, tools) in turns {
                        let record = scripted.user(text, tools).await?;
                        timings.add("pre_turn (log + recall)", record.pre_ms);
                        timings.add("post_turn (log)", record.post_ms);
                        pre_turn_timeouts += usize::from(record.timed_out);
                        if record.timed_out {
                            println!("   ! pre_turn timed out on {thread}");
                        } else if !record.logged {
                            println!("   ! a turn of {thread} was not logged");
                        }
                        tool_calls += record.tool_calls;
                        jobs.extend(record.jobs);
                        *writes.entry(tenant).or_default() += 1 + usize::from(record.logged);
                    }
                    let flushed = scripted.flush().await?;
                    *writes.entry(tenant).or_default() += flushed.logged;
                    for elapsed in flushed.completion_ms {
                        timings.add("pre_turn completed after deadline", elapsed);
                    }
                }
            }
        }

        // Settle: wait until every write is listed.
        let started = Instant::now();
        for (tenant, expected) in &writes {
            self.settle(
                &layout(run, scenario.name, tenant, self.pooled)?,
                *expected,
                settle_timeout(scenario.name),
            )
            .await?;
        }
        let settle_ms = ms(started);
        timings.add("settle (all writes listed)", settle_ms);

        let (ranked_ready, ranked_wait_ms) = if scenario.name == "needle_scale" {
            let started = Instant::now();
            let ready = loop {
                // The unique owner name checks indexing without warming a
                // scored question's exact query.
                let mut req = FetchRequest::new("Mira Solis", FetchMode::Hybrid, 10);
                req.filter = layout(run, scenario.name, MAIN, self.pooled)?.brain_filter(None);
                let fetch_started = Instant::now();
                let page = engine.fetch(req).await?;
                timings.add("scale direct fetch", ms(fetch_started));
                if page.hits.iter().any(|hit| hit.text.contains("Mira Solis")) {
                    break true;
                }
                if started.elapsed() >= Duration::from_secs(self.ranked_wait) {
                    break false;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            };
            (Some(ready), Some(ms(started)))
        } else {
            (None, None)
        };

        let mut probes = Vec::new();
        for probe in &scenario.probes {
            probes.push(self.probe(scenario, probe, "recall", timings).await?);
        }

        // The scale sweep measures retrieval over the planted corpus. Building
        // thousands of reference beliefs changes that corpus and obscures
        // the position/depth comparison.
        if scenario.name == "needle_scale" {
            if std::env::var("CORTEX_DB_KEEP").is_err() {
                engine
                    .forget(ForgetTarget::Filter(
                        layout(run, scenario.name, MAIN, self.pooled)?.holistic_filter(),
                    ))
                    .await?;
            }
            let usage = match (&self.inspector, usage_before) {
                (Some(inspector), Some(before)) => Some(inspector.usage().await?.since(&before)),
                _ => None,
            };
            return Ok(ScenarioReport {
                name: scenario.name,
                about: scenario.about,
                writes: writes.values().sum(),
                tool_calls,
                pre_turn_timeouts,
                settle_ms,
                synthesis: Synthesis::default(),
                probes,
                usage,
                ranked_ready,
                ranked_wait_ms,
            });
        }

        // Synthesis: the jobs the writes handed back, then one build per tenant
        // over its whole tree.
        let mut enrichment_drained = None;
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
            enrichment_drained = Some(quiet >= DRAINED_POLLS);
            println!(
                "   enrichment {} in {:.0} s",
                if quiet >= DRAINED_POLLS {
                    "drained"
                } else {
                    "hit wait cap"
                },
                waited / 1e3
            );
        } else if self.hosted && self.enrich_wait > 0 {
            // No queue to read (hosted): give enrichment its usual lag.
            tokio::time::sleep(Duration::from_secs(self.enrich_wait)).await;
            println!(
                "   waited {} s for enrichment (not observable)",
                self.enrich_wait
            );
        }
        for tenant in tenants(scenario) {
            let root = layout(run, scenario.name, tenant, self.pooled)?
                .root()
                .clone();
            jobs.push(BackgroundJob::BuildBeliefs {
                request: ConsolidateRequest::new(Reach::subtree(root)),
            });
        }
        let jobs = coalesce_builds(jobs);
        let runner = memory(MAIN, "eval")?.background();
        let mut synthesis = Synthesis {
            jobs: jobs.len(),
            enrichment_drained,
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
                let layout = layout(run, scenario.name, tenant, self.pooled)?;
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
                        layout(run, scenario.name, tenant, self.pooled)?.holistic_filter(),
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
            ranked_ready,
            ranked_wait_ms,
            name: scenario.name,
            about: scenario.about,
            writes: writes.values().sum(),
            tool_calls,
            pre_turn_timeouts,
            settle_ms,
            synthesis,
            probes,
        })
    }

    /// Waits until `layout` lists at least `expected` items.
    async fn settle(
        &self,
        layout: &MemoryLayout,
        expected: usize,
        timeout: Duration,
    ) -> Result<(), Error> {
        let started = Instant::now();
        loop {
            let mut listed = 0;
            let mut cursor = None;
            loop {
                let mut req =
                    ListRequest::new(layout.holistic_filter(), settle_page_size(expected));
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
            if started.elapsed() > timeout {
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
            layout(run, scenario.name, probe.tenant, self.pooled)?,
            probe.agent,
        )?
        .with_policy(policy.clone());
        let started = Instant::now();
        let mut timed_out = false;
        let mut host_elapsed = None;
        let mut completion_elapsed = None;
        let (markdown, tokens) = match &probe.via {
            Via::Ask => {
                let thread = format!("probe-{}", probe.id);
                let pre = PreTurn::new(thread, 0, probe.question);
                if self.openhuman {
                    let (pack, timeout, elapsed, completed) = host_probe_pack(
                        memory.clone(),
                        pre,
                        HostHook::for_turn(false, self.date_hint),
                    )
                    .await?;
                    timed_out = timeout;
                    host_elapsed = Some(elapsed);
                    completion_elapsed = completed;
                    pack
                } else {
                    pack(memory.pre_turn(pre).await?.pack)
                }
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
                if self.openhuman {
                    let (pack, timeout, elapsed, completed) = host_probe_pack(
                        memory.clone(),
                        pre,
                        HostHook::for_turn(true, self.date_hint),
                    )
                    .await?;
                    timed_out = timeout;
                    host_elapsed = Some(elapsed);
                    completion_elapsed = completed;
                    pack
                } else {
                    pack(memory.pre_turn(pre).await?.pack)
                }
            }
            Via::ContextDoc { heading } => {
                let root = layout(run, scenario.name, probe.tenant, self.pooled)?
                    .root()
                    .clone();
                let spec = ContextSpec {
                    briefs: vec![Brief::new(*heading, probe.question)],
                    reach: Some(Reach::subtree(root)),
                    ..ContextSpec::default()
                };
                let doc = context::compile(engine.as_ref(), &spec).await?;
                (doc.markdown, doc.tokens)
            }
        };
        let elapsed = host_elapsed.unwrap_or_else(|| ms(started));
        let mut result = score(scenario.name, phase, probe, &markdown, tokens, elapsed);
        result.timed_out = timed_out;
        if let Some(llm) = llm {
            let answer = llm.answer(&markdown, probe.question).await?;
            result.llm_ok = grade(probe, Some(&answer.text));
            result.llm_answer = Some(answer.text);
            result.llm_tokens = answer.tokens;
            result.llm_cost_usd = answer.cost_usd;
        }
        timings.add(&format!("probe {}", result.via), elapsed);
        if let Some(completed) = completion_elapsed {
            timings.add("probe pre_turn completed after deadline", completed);
        }
        let logged_turn = match &probe.via {
            Via::Ask => Some((format!("probe-{}", probe.id), 0)),
            Via::Continue {
                thread, turn_index, ..
            } => Some(((*thread).to_string(), *turn_index)),
            _ => None,
        };
        if let Some((thread_id, index)) = logged_turn {
            let mut filter = layout(run, scenario.name, probe.tenant, self.pooled)?
                .conversations_filter(Some(probe.agent));
            filter.thread_id = Some(thread_id);
            filter.turns = Some(TurnRange {
                first: index,
                last: index,
            });
            engine.forget(ForgetTarget::Filter(filter)).await?;
        }
        Ok(result)
    }
}

/// A pack's markdown and token count.
fn pack(pack: ContextPack) -> (String, usize) {
    (pack.markdown, pack.tokens)
}

/// Run a probe's pre-turn under the host deadline. A timed-out task still
/// finishes its write, but its pack is absent from the simulated prompt.
async fn host_probe_pack(
    memory: AgentMemory,
    pre: PreTurn,
    hook: HostHook,
) -> Result<((String, usize), bool, f64, Option<f64>), Error> {
    let started = Instant::now();
    let mut task = tokio::spawn(async move {
        let result = hook.run(memory, pre).await;
        (result, Instant::now())
    });
    match tokio::time::timeout(PRE_TURN_TIMEOUT, &mut task).await {
        Ok(context) => Ok((pack(context?.0?.pack), false, ms(started), None)),
        Err(_) => {
            let elapsed = ms(started);
            let (context, completed) = task.await?;
            context?;
            Ok((
                (String::new(), 0),
                true,
                elapsed,
                Some(completed.duration_since(started).as_secs_f64() * 1_000.0),
            ))
        }
    }
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
    println!("| Step | n | p50 | p95 | p99 | max |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for (step, samples) in &timings.0 {
        let l = Latency::of(samples);
        println!(
            "| {step} | {} | {:.1} | {:.1} | {:.1} | {:.1} |",
            l.n, l.p50, l.p95, l.p99, l.max
        );
    }

    println!("\n## Misses\n");
    for result in all.iter().filter(|p| {
        p.hit == Some(false)
            || p.answer_ok == Some(false)
            || p.llm_ok == Some(false)
            || p.leak
            || p.stale_first
            || p.timed_out
    }) {
        println!(
            "- {} {}/{} ({}, {}): hit {:?}, rank {:?}, stale first {}, leak {}, timeout {}; extractive {:?}; model {:?}",
            result.phase,
            result.scenario,
            result.id,
            result.via,
            result.style,
            result.hit,
            result.rank,
            result.stale_first,
            result.leak,
            result.timed_out,
            result
                .answer
                .as_deref()
                .map(|a| a.chars().take(90).collect::<String>()),
            result.llm_answer,
        );
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
