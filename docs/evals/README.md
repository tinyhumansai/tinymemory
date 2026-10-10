# Evals

Accuracy and latency measurements of TinyMemory against real engines, with
the method and the results of each recorded run. Tests prove behaviour; evals
measure how well that behaviour serves an agent.

| Eval | What it measures | Latest run |
| --- | --- | --- |
| [Agent memory](agent-memory.md) | The lifecycle (`pre_turn`, `post_turn`, `start_session`, compaction, belief builds) across twelve scenarios | 2026-10-04, CortexDB v0.10.4 |
| [CortexDB flags](cortex-flags.md) | Whether CortexDB's server flags move accuracy, learning, surprise, conflicts, cost and latency | 2026-10-04, CortexDB v0.10.4 |
| [OpenHuman host profile](openhuman-host.md) | OpenHuman turn deadline, logging parity and a 500-turn recall loop guard | 2026-10-10, reference and CortexDB |

## The agent memory eval

The harness is the `memory_eval` example in
`crates/tinymemory-integrations/examples/memory_eval/`:

| File | Role |
| --- | --- |
| `main.rs` | Runs each scenario: writes, settle, probe, synthesise, probe again, forget |
| `agent.rs` | The scripted agent: real lifecycle calls, scripted tool calls, an extractive "model" |
| `scenarios.rs` | The twelve scenarios and their probes |
| `score.rs` | Scoring a pack against a probe, totals and latency percentiles |
| `kpi.rs` | The run's KPIs: accuracy, learning, surprise, conflicts, cost, latency |
| `compare.rs` | `memory_eval compare`: the KPIs of several runs against a baseline |
| `inspect.rs` | Reads CortexDB's derived layers (facts, beliefs, conflicts) and model usage straight off the wire |
| `llm.rs` | The optional model that answers each probe from its pack (`--llm`) |
| `loop_guard.rs` | Replays the JSON-scripted 500-turn pack echo probe (`--loop-guard`) |

### Running it

```sh
# Offline, on the reference engine: seconds, no network.
cargo run -p tinymemory-integrations --features full --example memory_eval

# A throwaway CortexDB with deterministic mock models (needs Docker).
./scripts/memory-eval.sh

# The same with real models through OpenRouter (needs OPENROUTER_API_KEY),
# and a model answering every probe.
MODELS=openrouter ./scripts/memory-eval.sh --llm
```

**Hosted memory.** `--engine tinyhumans` runs the eval against CortexDB
behind the TinyHumans backend, with the production models it runs there:

```sh
TINYHUMANS_API_URL=https://api.tinyhumans.ai TINYHUMANS_API_KEY="YOUR_TEST_ACCOUNT_KEY" \
  cargo run -p tinymemory-integrations --features full --example memory_eval -- \
  --engine tinyhumans --llm --label hosted --json target/memory-eval/hosted.json
```

It is billed to the key's account, so use a test account. Hosted memory has
no admin routes, so the run cannot watch the enrichment queue (it waits
`--enrich-wait` seconds, default 60, before the belief build), and it reports
no usage or derived-layer inspection. Pass the key through the environment;
never commit it.

**Cost.** Real models are not cheap here. CortexDB runs its extraction
model over every stored event and again during belief builds. The report
ends with the usage CortexDB accounted for (calls, tokens and dollars as its
router prices them). Earlier runs did not record it, so their cost is
unknown. Run one scenario first (`--only <name>`) to gauge it. When the key
hits its spending limit, every embedding fails (`403 Key limit exceeded`)
and the run stops at the settle step with "only 0 of N writes visible".

The script prints the report and writes `target/memory-eval/<label>.md` and
`.json`. The JSON holds every probe's pack, so a miss can be read in full,
and the run's KPIs. Set `REUSE_IMAGE=1` to reuse an already-built local eval
image when Docker Hub is unavailable; the default builds it from the compose
file.

Flags (after `--`): `--engine reference|cortex|tinyhumans`, `--only <scenario>`,
`--enrich-wait <secs>`, `--json <path>`, `--label <name>`, `--llm`,
`--host openhuman`, `--brain-limit <n>`, and `--loop-guard`.
`--brain-limit` overrides the host's brain-document count and records it in
the JSON report for retrieval-depth sweeps. `CORTEX_DB_KEEP=1` leaves the
run's data in place for inspection.

### How a scenario runs

1. **Write.** Brain documents go in through `Brain::ingest`. Each scripted
   conversation runs through a `ScriptedAgent`, one exchange at a time:
   - `pre_turn` logs the user's turn and recalls a pack, with the last 8
     turns of the thread treated as still in the prompt;
   - the agent "calls" its scripted tools and writes their results into its
     reply (memory keeps only a call's name and id, so a result left out of
     the reply is lost);
   - `post_turn` logs the reply with its tool calls.

   Turns carry timestamps: each thread starts on its scenario day, and turns
   are a minute apart.
2. **Settle.** Wait until every write is listed (writes are `Accepted`, not
   indexed).
3. **Probe** (phase `recall`). Each probe asks a question through one
   lifecycle call: a new thread's `pre_turn`, `start_session`,
   `recall_for_compaction`, or the next `pre_turn` of a long thread. Each
   pack is then scored.
4. **Synthesise.** Wait until CortexDB's enrichment queue (fact extraction)
   drains, at most `--enrich-wait` seconds (600 by default). With a real
   model this takes about two minutes per scenario, and beliefs are built
   from extracted facts, so building any sooner builds from nothing. Then
   run every job the writes handed back, plus one `BuildBeliefs` over each
   tenant's whole tree. Recorded runs before 2026-10-04 15:00 UTC waited a
   fixed 20 s instead.
   - CortexDB's extraction needs a model that returns JSON within a small
     token limit. A model whose reasoning cannot be turned off
     (`z-ai/glm-5.3-flash`) spends the limit thinking and extracts
     nothing; `z-ai/glm-4.7-flash` works.
5. **Probe again** (phase `synthesis`), then forget everything.

### Metrics

Every check is a case-insensitive substring match on the pack's markdown.

| Metric | Meaning |
| --- | --- |
| Pack hit | Every expected string is in the pack |
| MRR | Mean of 1 / (position of the first expected string). Positions count bullets and prose paragraphs in reading order |
| Extractive answer | The scripted agent's answer (the pack line sharing the most words with the question) holds every expected string and no superseded one |
| Model answer | With `--llm`, a model answering from the pack alone (temperature 0, `openai/gpt-4.1-mini` by default) is graded the same way |
| Fresh first | Over probes whose fact changed: the current value is present and comes before every superseded one |
| Captured | Synthesis phase on CortexDB: a fact or belief CortexDB derived holds the expected answer, whether or not the pack shows it |
| Leaks | Probes whose pack holds a forbidden string (another tenant's data, or turns still in the prompt), over the probes that check |

The report ends with the run's **KPIs**, one number per question a
configuration is chosen on (accuracy, learning, surprise, conflicts, cost,
latency). They are defined in [cortex-flags.md](cortex-flags.md#kpis).

Probes are tagged **lexical** when the question shares its key words with
the stored text, and **paraphrase** when only meaning connects them. The
extractive answer cannot answer a paraphrase by construction, so compare
the model answer there.

### Caveats

- The mock models (`integration/cortexdb/mock_inference.py`) embed by
  hashing and extract nothing. Against them CortexDB ranks only by keyword,
  and synthesis builds nothing. They measure wiring and latency, not
  quality.
- 38 scored probes is a smoke-sized sample: one probe is about 3 points.
  Read a difference of a probe or two as noise.
- Latency runs against a local Docker server. The real-model numbers include
  OpenRouter round trips for every query embedding.
