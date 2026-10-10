# OpenHuman host profile

`memory_eval --host openhuman` replays the existing scripted scenarios through
the parts of OpenHuman's turn hook that affect the hot path. It uses the same
engine and probes as the default profile, so pack accuracy can be compared
without changing the stored fixture data. It is a mirror inside TinyMemory;
there is no dependency on `openhuman-core`.

Use `--layout v3 --team-limit 3` to measure cross-agent recall from pooled
chats on CortexDB's per-person scope tree. The host mirror defaults to
`team_limit = 0` for historical comparison; the JSON report records the layout
and team limit so those runs cannot be mistaken for the new OpenHuman default.

## V3 pooled team recall, 2026-10-10

The team section now reads the same `ws:main` conversation scope as history,
excluding the current agent's turns before applying its limit. The CortexDB
v0.10.4 runs below used fresh collections for each setting, the same 5-second
OpenHuman deadline, and one team limit of three turns. The full mock run has
58 scored probes per phase; the live OpenRouter slice covers the three
`team_handoff` and four `conflicts` probes. Each result was the same in recall
and synthesis.

| Run | Team limit | Pack hits | Extractive answers | Model answers | Probe timeouts |
| --- | ---: | ---: | ---: | ---: | ---: |
| Full mock v3 | 0 | 48/58 | 31/58 | — | 0 |
| Full mock v3 | 3 | 51/58 | 34/58 | — | 0 |
| Live v3 slice | 0 | 3/7 | 1/7 | 2/7 | 0 |
| Live v3 slice | 3 | 7/7 | 4/7 | 5/7 | 0 |

In the full mock run, `team_handoff/duplicate-invoices`,
`team_handoff/account-id`, and `conflicts/promise` changed from misses to
hits; no scored probe moved the other way. The mock paraphrase
`team_handoff/billed-twice` still misses. With real embeddings it reaches the
pack, but the model slice remains only seven questions and one fresh run per
setting. One baseline model answer graded correct despite an empty pack, so
model scores must be read beside pack hits. In the final rerun, CortexDB
model cost was $0.0196 with team recall off and $0.0206 with it on; that
difference is noise at this sample size.

The source references below use OpenHuman commit `cf16716f4f` for the hook
behavior, [OpenHuman PR #7346](https://github.com/tinyhumansai/openhuman/pull/7346)
for the 5000 ms deadline, and
[OpenHuman PR #7352](https://github.com/tinyhumansai/openhuman/pull/7352)
for the proposed three-turn team default. Update them when the host's
defaults or hook behavior changes.

| Behavior | Eval mirror | OpenHuman source |
| --- | --- | --- |
| 1200-token budget, 8 learnings, 6 brain, 6 history, 3 team, beliefs every 10 turns | `main.rs`, OpenHuman policy with `--team-limit 3` | `crates/openhuman-core/src/config/schema/memory.rs:278-336` |
| Plain `pre_turn` by default, resumed hook after compaction, optional dated path with `--date-hint`, empty pack after 5000 ms | `agent.rs`, `HostHook` | `crates/openhuman-core/src/config/schema/memory.rs` and `crates/openhuman-core/src/memory/lifecycle/hooks.rs` |
| Timed-out task continues in the background and can still log the turn | `agent.rs`, `ScriptedAgent::flush` | `crates/openhuman-core/src/memory/lifecycle/hooks.rs:190-225` |
| Identical pending belief builds run once | `main.rs`, `coalesce_builds` | `crates/openhuman-core/src/memory/lifecycle/jobs.rs`, `enqueue` |
| User and reply indices `2n` and `2n+1` | `agent.rs`, `ScriptedAgent::user` | `crates/openhuman-core/src/memory/lifecycle/hooks.rs:245-255,414-435` |
| Tool-result lines, whitespace folding, 240-character cap | `agent.rs`, `logged_reply` | `crates/openhuman-core/src/memory/lifecycle/hooks.rs:385-406` |

The profile reports pre-turn p50, p95, p99 and timeout rate. These host-facing
latencies stop at the deadline; separate completion samples show how long
timed-out work actually took. The report also records per-section recall,
scope discovery, CortexDB fetch, and per-scope recall-pack timings. The
recall-pack timing includes server embedding and ranking; CortexDB does not
expose those stages separately through this wire. A timeout leaves
the simulated model with an empty pack. `--loop-guard` reads its script from
`examples/memory_eval/data/loop_guard.json`, replays 500 turns, and exports
all stored items to detect any `<memory-context>` tag copied into memory. It
also reports pack size and repeated bullet-line rates for the first and last
50 turns. The command fails if any stored item contains the tag.

```sh
cargo run -p tinymemory-integrations --features full --example memory_eval -- \
  --engine reference --host openhuman --only none --loop-guard \
  --json target/memory-eval/openhuman-loop-reference.json

./scripts/memory-eval.sh --host openhuman --only tool_heavy,compaction \
  --json target/memory-eval/openhuman-cortex.json

cargo run -p tinymemory-integrations --features full --example memory_eval -- \
  --engine reference --host openhuman --scale-events 1000 \
  --scale-position middle --seed 251 --json target/memory-eval/scale.json

./scripts/memory-eval.sh --host openhuman --only none --safety-audit
```

The 500-turn loop guard is separate from the standard scenario run because it
adds 1000 stored turns and many belief builds. The profile currently uses the
scripted answerer and the existing Rust scenario definitions. The live model
step, memory-tool calls, persisted job queue, ingestion/import paths, JSON
scenario format for the full suite, and 8000 ms compaction deadline in issue
[#251](https://github.com/tinyhumansai/tinymemory/issues/251) remain to be
implemented. The OpenHuman scrub guard is also outside this profile; the
scripted fixtures contain no sensitive data.

The scale sweep accepts 100, 1,000, or 10,000 seeded documents and early,
middle, or late needle placement. It measures retrieval before synthesis so
belief builds do not change the planted corpus. A direct fetch first checks
whether the answer-bearing document is ranked at all; `--ranked-wait 30`
polls for up to 30 seconds and records that lag separately from the host
probe. Readiness uses the planted owner's name rather than either scored
question, so it does not warm their exact queries. The 200-turn
`long_compaction` case is opt-in with `--only long_compaction`; ordinary full
runs include the shorter coding and task-drift cases. Probe turns are removed
after each score so they cannot contaminate later probes or synthesis.
`--safety-audit` checks synthetic tenant isolation and forget/erase across
packs, direct answers, `context.md`, export, and the derived layers available
on direct CortexDB. Set `BUDGET_USD=5` for a sequence of OpenRouter runs in
one `OUT_DIR`; the runner tracks the key's cumulative usage and reserves
`COST_PER_RUN` (default $0.50) before starting each run.

## Measured runs, 2026-10-10

The tables below were collected with OpenHuman's former 1500 ms default.
The current host and this mirror use 5000 ms; fresh runs must be compared
with their recorded deadline rather than treating old timeout rates as current.

| Run | Scope | Recall / synthesis pack hit | Synthesis model answer | Pre-turn p95 | Timeout rate | CortexDB cost |
| --- | --- | --- | --- | --- | --- | --- |
| `openhuman-cortex-mock` | `tool_heavy,compaction`, 8 probes | 8/8 · 8/8 | no answer model | 48 ms | 0/27 scripted turns | $0.000 |
| `openhuman-cortex-mock-full` | all 12 scenarios, 53 probes | 41/52 · 40/52 | no answer model | 358 ms | 0/175 calls | $0.000 |
| `openhuman-cortex-openrouter` repeat 1 | `tool_heavy`, 6 probes | 0/6 · 5/6 | 4/6 | 1502 ms | 10/19 calls | $0.050 |
| repeat 2 | same | 0/6 · 5/6 | 5/6 | 1502 ms | 11/19 calls | $0.012 |
| repeat 3 | same | 0/6 · 5/6 | 4/6 | 1508 ms | 12/19 calls | $0.030 |

The historical three live runs used CortexDB v0.10.4, OpenRouter embeddings and
extraction, and `openai/gpt-4.1-mini` answers. The answer model cost another
$0.002 per run. The timeout rate counts seven scripted turns and twelve
pre-turn probes across both phases. All six recall-phase probes missed the
deadline in every repeat. The 19-call sample gives a timeout range of
53–63%, well above the proposed 1% gate. The synthesis pack found five of
six expected answers each time; the absent one was the paraphrase asking who
to contact about the regression. CortexDB model cost varied from $0.012 to
$0.050, so a single run would hide substantial cost noise.

Those runs used dated recall even though OpenHuman defaults to plain recall,
and their probes logged questions back into the measured corpus. The
corrected runs below supersede these figures for comparisons.

The reference-engine loop guard ran all 500 turns with 500 nonempty packs,
zero timeouts and zero stored `<memory-context>` tags. Mean pack size was
114 tokens in the first 50 turns and 130 in the last 50. Repeated bullet-line
rates were 82.4% and 85.7%; this fixture intentionally repeats the same
acknowledgement, so the rate is a measurement of that repetition rather than
a leakage count.

## Expanded benchmark slice

The corrected full mock run covered 14 default scenarios and 58 scored
probes per phase. Packs contained the expected text for 48/58 (83%) in both
phases; the extractive answerer answered 28/58 (48%) correctly. It recorded
zero host timeouts across 195 calls; pre-turn p50/p95/p99 were 30/76/144 ms.
The three new coding probes and three task-drift probes
each had 3/3 pack hits and 1/3 extractive answers. These separate retrieval
from answer selection. Mock inference still derives no facts or beliefs, so
its captured and synthesis-gain figures are not live model scores. The
200-turn compaction case is opt-in: on the reference engine it returned the
archive key in all three packs. Coalescing identical pending belief builds
reduced synthesis from 41 jobs and 85 seconds to 2 jobs and 4.1 seconds. It
remains opt-in because a live 200-turn run would dominate ordinary sweeps.

The seeded reference-engine retrieval sweep used one answer-bearing document
among otherwise deterministic near duplicates. Every lexical probe hit at
rank 1. Every paraphrase probe missed; the reference engine uses a toy
keyword/vector scorer, so this is an offline calibration result, not a live
semantic-retrieval estimate.

| Events | Early lexical / paraphrase | Middle | Late | Probe time range |
| ---: | --- | --- | --- | ---: |
| 100 | 1/1 · 0/1 | 1/1 · 0/1 | 1/1 · 0/1 | 5 ms |
| 1,000 | 1/1 · 0/1 | 1/1 · 0/1 | 1/1 · 0/1 | 40–49 ms |
| 10,000 | 1/1 · 0/1 | 1/1 · 0/1 | 1/1 · 0/1 | 457–1,079 ms |

The 10,000-item setup exposed quadratic duplicate checking in the reference
engine's default `store_many` path. Its batch override now computes held IDs
once per batch; the 10,000-item accepted writes took 6.8–7.5 seconds. Listing
all writes for settlement took another 14–17 seconds. Neither is included in
the probe latency column.

Three fresh CortexDB v0.10.4/OpenRouter `tool_heavy` repeats used the corrected
plain pre-turn path, deleted each probe's logged question before the next
score, and waited for enrichment. The six scored questions and model settings
were the same as the historical runs above.

| Repeat | Initial / synthesis pack hit | Synthesis model answer | Pre-turn timeouts | CortexDB model cost |
| --- | ---: | ---: | ---: | ---: |
| 1 | 3/6 · 6/6 | 5/6 | 7/19 | $0.031 |
| 2 | 1/6 · 6/6 | 6/6 | 10/19 | $0.025 |
| 3 | 4/6 · 6/6 | 6/6 | 6/19 | $0.017 |

Initial recall still varies substantially and misses the 1500 ms deadline
in 32–53% of calls. All three synthesis packs now include `jmiller` for the
formerly consistent `who-to-ask` miss; removing probe contamination is a
plausible contributor, but these small runs do not isolate its effect from
model variability. The third repeat's stage timings show the bottleneck:
57 scope discoveries had p95 13 ms, while 57 CortexDB recall packs had p95
2233 ms. End-to-end Cortex fetch p95 was 2235 ms. The pack request includes
server embedding and ranking, which this wire cannot break out further.

The live safety audit passed 20/20 sibling-tenant, forget, and erase checks
after waiting for a derived record before both deletion operations. It also
confirmed that the restore between them created a new record. An earlier
immediate forget run reported a derived residual while every public item
channel was clear; a second immediate run and the controlled runs passed.
That transient needs a dedicated extraction-versus-forget race reproduction
before claiming the derived layer is always clear at return.

The first three live 100-document scale runs probed immediately after all
writes appeared in `list`. None of their six packs contained the needle; each
returned only `Case 0`. A controlled repeat that polled direct ranked fetch
found the needle after 5.5 seconds (three attempts). Its lexical host probe
then hit, while the paraphrase probe still missed. Three further fresh
100-document collections used the corrected readiness query. Direct fetch
found the needle after 19.8, 15.1, and 15.9 seconds, respectively. All six
host probes then reached the 1.5-second deadline and received empty packs;
their CortexDB model costs were $0.018, $0.006, and $0.006. This distinguishes
write-to-ranked lag from host-deadline and semantic-retrieval failures. The
`--ranked-wait` report records both whether the direct fetch became ready and
how long it waited; a pack miss after readiness is a separate failure.
The first 1,000-document live attempt exposed a second lag: only 645 writes
were listable at the ordinary 90-second settlement cap. Scale runs now wait
up to 300 seconds, list up to 1,000 items per page, and record that wait
separately from ranked readiness and host probe latency. Three fresh live
1,000-document repeats with a 30-second direct-fetch readiness window were
still unstable:

| Repeat | List settlement | Ranked readiness wait | Direct fetch ready | Lexical / paraphrase pack | Probe timeouts | CortexDB cost |
| --- | ---: | ---: | --- | --- | ---: | ---: |
| 1 | 19.1 s | 8.3 s | yes | 1/1 · 0/1 | 0/2 | $0.103 |
| 2 | 31.7 s | 1.5 s | yes | 0/1 · 0/1 | 2/2 | $0.099 |
| 3 | 0.4 s | 31.6 s | no | 0/1 · 0/1 | 2/2 | $0.097 |

These are separate fresh collections, not repeated reads of one index. The
first repeat returned the lexical answer in 1.14 seconds and the paraphrase
missed at 1.37 seconds. Both probes in repeats 2 and 3 reached OpenHuman's
1.5-second deadline. A visible write therefore does not guarantee prompt
ranked retrieval, and even direct-fetch readiness did not guarantee that a
host probe would complete within its deadline. The per-run model cost includes
background extraction and enrichment and is not a per-query price.

## Accuracy follow-up

On the same mock CortexDB fixture, with `openai/gpt-4.1-mini` answering from
the pack, the `coding_session,task_drift` slice kept all 6/6 pack hits per
phase. Model answers rose from 5/6 to 6/6 in recall and from 5/6 to 6/6 in
synthesis after agent-history turns were presented newest first within each
thread. The corrected answer names `test_does_not_repeat_unknown_write` as the
test that exposed the unsafe retry. Previously the model chose the older
`test_retries_refused_connection` result. This is a six-question slice, not a
full-suite accuracy estimate.

A fresh live 100-document scale run at the 5000 ms deadline returned its
lexical answer in 2.08 seconds and its paraphrase miss in 1.78 seconds, with
no timeouts. The correct document ranked first for the lexical question but
23rd for the paraphrase in a direct 100-event CortexDB recall. This run shows
that the longer deadline removes the timeout for these two probes, while the
paraphrase still needs a retrieval-quality improvement. It is a fresh
collection and cannot be treated as a paired timeout comparison with the
1500 ms runs above.

## Accuracy follow-up after #255

Fresh CortexDB v0.10.4/OpenRouter runs used the same 100-document middle
needle fixture, seed 251, v3 layout, three team turns, 5000 ms deadline, and
30-second ranked-readiness wait. Each run had a new collection. The literal
owner query hit and the semantic paraphrase missed in all three settings:

| Setting | Pack hits | Paraphrase | Mean pack tokens | Probe timeout |
| --- | ---: | ---: | ---: | ---: |
| Default server, six brain documents | 1/2 | miss | 130 | 0/2 |
| Max Recall server flags, six brain documents | 1/2 | miss | 130 | 0/2 |
| Default server, 24 brain documents | 1/2 | miss | 512 | 0/2 |

The Max Recall profile enables three HyDE passages, entity-vector seeding,
assistant excerpts, and enforced bi-temporal validity. Neither it nor a
fourfold increase in the displayed brain documents recovered this fixture's
paraphrase. The wider section spent almost four times the context tokens, so
the production six-document default stays unchanged. `--brain-limit <n>` now
allows another depth to be measured without editing that default; reports
record the chosen value.

The same seven-question `team_handoff,conflicts` live slice exposed an answer
problem after retrieval: all seven packs held their expected evidence, but
the old benchmark model prompt answered only 5/7 correctly. Both misses
chose the undated five-day refund document and omitted a conversation saying
Finance had changed refunds to ten days. The final prompt asks the model to
distinguish an unresolved conflict (undated policy versus dated report) from
an explicit update of an older dated value. Reports record its version as
`source-reconciliation-v2`.

| Fresh run | Pack hit | Model answer, recall | Model answer, synthesis | Timeouts |
| --- | ---: | ---: | ---: | ---: |
| Live slice, original prompt | 7/7 | 5/7 | 5/7 | 0 |
| Live slice, source reconciliation | 7/7 | 7/7 | 6/7 | 0 |
| Full mock, first conflict prompt | 51/58 | 43/58 | 44/58 | 0 |
| Full mock, source reconciliation | 51/58 | 50/58 | 50/58 | 0 |

The remaining live synthesis miss named Acme's current Team plan but also
mentioned the superseded Enterprise plan, which the strict answer grade
rejects. Mock extraction stayed 34/58 in both full runs. The full mock uses
keyword-based model doubles, so its pack misses do not estimate live semantic
retrieval. These are small fresh runs, not a statistical confidence interval.
The prompt changes the eval answerer only; it does not change TinyMemory's
stored memory or OpenHuman's production answer instructions.
