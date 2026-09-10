# CortexDB provider

This module adapts CortexDB's native v1 experience, recall, and answer routes to
TinyMemory's provider contracts. `CortexProvider` combines the mandatory
key/value, recall, and portability surface with document, conversation,
learning, raw-event, and grounded-answer capabilities.

`operations.rs` owns protocol translation and the capability implementations.
Product writes become indexed CortexDB experiences whose private envelope keeps
the original TinyMemory payload, logical key, category, session, and taint.
`types.rs` contains the private request input shared by those translations, and
`test.rs` covers response validation and bounded conversions.

Documents and conversation messages derive idempotency from their complete
lossless payload; conversation identities additionally include message
position. This makes exact retries safe without reusing an idempotency key for a
different CortexDB request body, which CortexDB rejects as a conflict. Raw
events use their namespace and event id so changing an event body under the same
host identity is surfaced as a conflict.

Writes wait for CortexDB's indexed barrier and are attempted once because an
ambiguous retry could duplicate an append. Recall may retry transient failures.
Answering first creates a retryable recall pack, then invokes synthesis exactly
once with that pack id because inference can be billed and nondeterministic.

Bearer credentials are allowed over HTTPS and literal loopback HTTP only. The
provider rejects credentialed cleartext endpoints before constructing the
client, does not render credentials through `Debug`, maps missing namespaces to
TinyMemory's global namespace, and rejects answer filters CortexDB cannot apply
safely.
