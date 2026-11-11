# Single-name memory tool

SingleTool presents recall, fetch, learn and forget under one configurable
model-facing name. The host maps its name, description, parameter schema,
permission class and concurrency class to the host tool runtime.

Hosts that can accept writes before an engine is bound may use
`SingleTool::queued(name, scope, sink)`. That facade advertises only learn and
forget, reuses the same trusted metadata and WriteSink path, and returns
`Unsupported` for recall and fetch until an engine-backed `SingleTool::new`
can be constructed.

## Scope and reads

MemoryTools supplies the engine and trusted ToolScope. Reads share the
seven-tool request shaping and host defaults. SingleTool accepts the full
TinyMemory MetaFilter and replaces its reach with the trusted scope; the
seven-tool facade retains its stricter filter parser. Recall runs in evidence
mode, ignores model-supplied instructions and limits, caps rendered evidence
near 2,500 tokens at line boundaries, and leaves the current thread's prompt
window out. A missing or mismatched window falls back to excluding the whole
current thread. Fetch is fixed to five items per page and preserves the engine
cursor while defaulting to the first engine-declared mode.

ToolScope carries trusted workspace, thread, agent and tool-call attribution.
Learn ignores model-supplied metadata and stamps the trusted ToolCallRef,
SourceKind::Agent, namespace and other available scope facts. It uses the
configured tool name only when the scope has no tool-call reference. Read-only
scopes reject learn and forget before consulting the sink.

## Pending write adapter

Learn and forget require a WriteSink, the host's prequeue policy hook.
PendingWriteSink adapts one existing PendingWrites queue using the host's
ListStore, Binding and WriteGate. It derives the persisted scope root from
trusted reach.at, falling back to place for an unrestricted single-tenant
scope. The adapter does not scrub learning content or apply host policy; the
host must wrap it with existing scrub and policy checks before forwarding
writes. A successful Ack means only that the operation is durable locally.

The host owns the queue partition, drain scheduler and executor, engine events,
settled callbacks such as cache invalidation, and the exclusive owner barrier
that makes erasure wait for a write already in flight. The base queue gate
stops later drain iterations but does not cancel an active executor. The
adapter does not spawn drains and never performs a direct engine write.
