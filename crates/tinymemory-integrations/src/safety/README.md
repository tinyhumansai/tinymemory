# Safety decorators

The `safety` feature provides `scrub_item` for individual items and
`ScrubbingEngine` for any `MemoryEngine`. The scrubber runs locally under a
`tinyredact::Policy`; it makes no network calls. It removes credentials and
personal identifiers from document titles and text, conversation turns,
learning text and evidence, and metadata URLs. Metadata identifiers and URI
bodies are left intact because identifiers are used for filtering and sources
resolve URIs before storing.

`ScrubbingEngine` explicitly forwards every current `MemoryEngine` method.
It applies `scrub_item_with` to `store`, `store_with`, `store_many` and
`store_many_with`, and passes options and reaches unchanged. In particular,
`forget_within` is forwarded to the wrapped engine rather than using the trait
default, which could expand the lookup beyond the requested reach.

`TimedEngine` logs one `[memory:timing]` debug line for each engine operation.
It includes the operation name, engine id, elapsed milliseconds and outcome;
successful reads and writes also include a result count. Failed outcomes use
`tinymemory_api::Error::code()`. Error messages, item text and request
parameters are not logged. Health lines expose only `ok`, `degraded` or `down`,
never the engine-provided reason.

The Cortex registry's `EngineBinder` composes both decorators for configured,
host-installed and transient external engines. A host should use the binder
for its engine paths instead of manually wrapping individual call sites. The
binder is enabled by the `cortex` feature, which implies `safety`; a build
enabling `safety` alone can use the decorators directly.
