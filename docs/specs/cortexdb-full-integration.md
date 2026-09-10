# CortexDB full integration

## Purpose

The `cortex` remote driver must expose CortexDB as more than a generic keyed
memory store. In addition to TinyMemory's mandatory Core, Recall, and
Portability families, it serves document, conversation, learning, raw-event,
and grounded-answer operations through CortexDB's native v1 API.

## Contract

- `cortex_provider` returns a provider advertising `DocumentIngest`,
  `ConversationIngest`, `LearningIngest`, `EventIngest`, and `Answer` in
  addition to the mandatory families.
- Documents use the `document` modality. Conversation messages use the
  `conversation` modality and retain their order and speaker. Learnings use
  `observation`. Raw events use their open `event_type` as the modality.
- Tool calls are raw events whose `event_type` is `tool_call`; their structured
  arguments, outcome, and tool identity remain in `RawMemoryEvent::metadata`.
- Every product-facing write preserves the logical namespace, original payload,
  session, timestamp, source identity, metadata, and provenance taint in the
  adapter's private envelope. CortexDB receives the human-readable content for
  indexing and extraction.
- Product writes wait for the indexed barrier. Repeating the same event id and
  body is idempotent; reusing an event id for a different body is a conflict.
- Learning observation times become CortexDB event times rather than being
  replaced by ingestion time.
- Recall returns both records written through Core and records written through
  the granular ingestion families, in CortexDB's rank order.
- Answers are produced by CortexDB's `/v1/answer` route and include the returned
  citations and the model named by `diagnostics.answer_model`. A missing
  namespace means TinyMemory's global namespace, and the stratified layer caps
  sum to the caller's total answer limit.
- Credentialed CortexDB endpoints require HTTPS except for literal loopback
  endpoints used by local development and the test harness.
- Credentials are accepted at construction, never rendered by `Debug`, and
  never included in errors or simulation output.

## Local full-memory profile

The repository supplies a CortexDB v0.9.9 Docker profile. It enables the
memory-pipeline features that an OpenAI-compatible Ladder can serve: 3072
dimension embeddings, extraction, enrichment, entity graph, HyDE, multihop,
entity-vector seeding, temporal fact handling, answers, verification,
consolidation, and background scheduling.

The live profile routes `vectors`, `flash`, `reasoning`, and `max-reasoning` to
the host's Ladder at `http://host.docker.internal:6969/v1`. Cohere-only
reranking, binary media processors, connectors, compliance infrastructure, and
the code-intelligence plane are outside this integration.

## Verification

CI runs the same CortexDB image against a deterministic OpenAI-compatible
inference fixture. A separate opt-in live mode uses the real local Ladder. Both
ingest a document, an ordered conversation, a learning, a generic event, and a
tool-call event, then prove recall, answer citations, scope isolation,
idempotency, and persistence across a CortexDB restart.
Persistence is checked by recalling a known record after the restart, not only
by observing that the scope name survived.
