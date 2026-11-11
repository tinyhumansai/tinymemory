# Architecture

How TinyMemory is built, one document per concern. The accepted behaviour (the
"what and why") is [`specs/memory-v2.md`](../specs/memory-v2.md) and, for the
agent lifecycle, [`specs/agent-memory.md`](../specs/agent-memory.md); these pages
describe the shape of the code that delivers it. Item-level reference lives in
rustdoc next to the code.

| Document | Read it for |
| --- | --- |
| [overview.md](overview.md) | The memory crates, standalone redaction library, dependency graph, feature map, and end-to-end write and read paths |
| [api.md](api.md) | The core contract: the `MemoryEngine` trait, descriptor, health, errors, limits and the wire format |
| [api-items.md](api-items.md) | Items, metadata and filters in detail: every field, validation rule and JSON shape |
| [operations.md](operations.md) | Step-by-step semantics of store, store_many, fetch, recall, list, forget, explore and get |
| [namespaces.md](namespaces.md) | The memory tree: `Namespace`, `Segment`, `Reach`, and what each operation does with them |
| [cortex.md](cortex.md) | The CortexDB engine: wires, scopes, envelopes, recall |
| [cortex-wire.md](cortex-wire.md), [cortex-wire-envelope.md](cortex-wire-envelope.md), [cortex-labels.md](cortex-labels.md), [cortex-flows.md](cortex-flows.md), [cortex-chunks.md](cortex-chunks.md), [cortex-local-paths.md](cortex-local-paths.md), [cortex-layout.md](cortex-layout.md) | The CortexDB wire formats, the lookup and readable labels, the step-by-step request flows, chunked documents, why no local path is sent, and the scope layouts (legacy and v3) |
| [tools.md](tools.md) | `tinymemory-tools`: the seven agent tools, host-fixed scoping, `context.md` |
| [lifecycle.md](lifecycle.md) | The agent memory lifecycle: the standard layout (brain, conversations, learnings), holistic recall, pre- and post-turn, compaction, background belief builds |
| [integrations.md](integrations.md) | `tinymemory-integrations`: registry and config, documents, sources, safety, legacy import |
| [testing.md](testing.md) | The conformance suite, the reference engine and the test layout |

Reading order for a newcomer: overview, then api, then operations. Read
namespaces before writing anything that serves more than one agent.
