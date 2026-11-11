# CortexDB lookup labels and digests

The digest labels every CortexDB event carries for server-side narrowing.
The rest of the wire is in [cortex-wire.md](cortex-wire.md).

Each event carries up to ten `context.labels`, each `tm:<tag>:` followed by
the first 16 lowercase hex digits (64 bits) of the SHA-256 of the value:

| Label | Value hashed | On |
| --- | --- | --- |
| `tm:i:` | the item id | every event |
| `tm:t:` | `meta.thread_id` | when set |
| `tm:s:` | `meta.source.id` | when set |
| `tm:r:` | `meta.repo` | when set |
| `tm:w:` | `meta.workspace` (kept even when the envelope drops it) | when set |
| `tm:a:` | `meta.agent_id` | when set |
| `tm:l:` | `meta.language` | when set |
| `tm:c:` | `meta.channel` | when set |
| `tm:cw:` | `meta.channel` + NUL + workspace identity | when both are set |
| `tm:k:` | the source kind (`meta.source.kind`) | every event |

A label holds a **digest**, not the value, because the engine splits a label
filter on commas and bounds a label's length, and a path or source id may be
long or hold a comma.

For `tm:cw:`, workspace identity is the workspace value when it is not an
absolute local path. For an absolute path it is the full 40-hex `path_digest`
already kept in the envelope's private path metadata. The query computes the
same identity before hashing the composite, so the workspace path never enters
the event labels.

Reads use labels two ways:

- **Item lookup.** Replay detection, `get`, conversation assembly and forget
  by id ask the listing for `tm:i:<digest(id)>` labels. Because a label is a
  digest, every hit is re-checked against the envelope's real `id`.
- **Narrowing.** A read whose filter has a labelled field sends **one** label
  filter to narrow server-side: the first set field of thread, source id, repo,
  workspace, agent, language (in that order, most selective first), else the
  filter's source kinds (several `tm:k:` labels, which the engine reads as
  any-of). `channel` is deliberately absent from this legacy narrowing path:
  records written before the channel labels existed still need to be found by
  the broad field. Channel recall supplements that broad pack with a second
  pack narrowed by `tm:c:` or, for a workspace filter, `tm:cw:`. Only labels
  of one field may be sent together, since the engine keeps events carrying
  *any* of the labels.

The label only ever narrows. Every reader **always** re-applies the full
`MetaFilter` to the decoded envelope, so a digest collision costs a wasted row
and never a wrong answer. `folder` and `file_path` match as prefixes, which a
label cannot, so they are filtered client-side only. No local path is sent:
see [cortex-local-paths.md](cortex-local-paths.md).

Channel labels are additive lookup hints. Existing events without `tm:c:` or
`tm:cw:` remain in the broad pack and are still filtered by their envelope's
`meta.channel`; no stored event needs rewriting.

## Readable labels

Beside the lookup labels, a v3 event carries labels a person browsing the
events can read. They are never filtered on.

| Label | When |
| --- | --- |
| `kind:<kind>` | every event |
| `agent:<agent_id>` | `meta.agent_id` is set |
| `thread:<thread_id>` | `meta.thread_id` is an app thread, `thread-<uuid>` (lowercase hex). A channel's thread (`channel:…`) never gets one: it can name the person at the other end |
| `file:<name>`, `page:<n>[-<m>]`, `section:<title>` | a document, a piece |
