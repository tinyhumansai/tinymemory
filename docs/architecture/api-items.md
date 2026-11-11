# Items, metadata and filters

The data model of [`tinymemory-api`](api.md): what is stored (`StoreItem`),
what describes it (`MemoryMeta`) and how reads select it (`MetaFilter`).

## `StoreItem`

The unit of `MemoryEngine::store`. Three variants, internally tagged by
`"type"` (`document`, `conversation`, `learning`). Each carries a `meta`
(`#[serde(default)]`, so it may be omitted on the wire).

### Document

| Field | Type | Notes |
| --- | --- | --- |
| `title` | `Option<String>` | Omitted when `None`. |
| `body` | `DocumentBody` | `Text(String)` or `Uri(String)`; must be `Text` when it reaches an engine. |
| `mime` | `Option<String>` | The body's MIME type, when known. |
| `meta` | `MemoryMeta` | |

```json
{
  "type": "document",
  "title": "Ownership",
  "body": { "text": "Ownership moves values." },
  "mime": "text/markdown",
  "meta": {
    "source": { "kind": "folder", "id": "notes" },
    "file_path": "/notes/rust/ownership.md",
    "language": "en"
  }
}
```

`DocumentBody::Uri` serialises as `{ "uri": "https://..." }`. It exists so a
source can describe a document before reading it; `validate` refuses it.

### Conversation

`turns: Vec<Turn>`, in order. A `Turn` is `{ role, text, at?, tool_calls }`:
`role` is `user | assistant | system | tool`; `at` is an RFC 3339 instant;
`tool_calls` (`Vec<ToolCallRef>`, omitted when empty) lists the calls the turn
made.

```json
{
  "type": "conversation",
  "turns": [
    { "role": "user", "text": "Which port does the API use?", "at": "2026-10-04T09:30:00Z" },
    {
      "role": "assistant",
      "text": "8080.",
      "tool_calls": [{ "name": "read_config", "id": "call_1" }]
    }
  ],
  "meta": { "source": { "kind": "conversation", "id": "thread-42" }, "thread_id": "thread-42" }
}
```

### Learning

| Field | Type | Notes |
| --- | --- | --- |
| `text` | `String` | The statement. |
| `kind` | `LearningKind` | `preference | fact | procedure | correction | other`. |
| `confidence` | `f32` | `0.0..=1.0`. |
| `evidence` | `Option<String>` | What supports it; omitted when `None`. |
| `meta` | `MemoryMeta` | |

```json
{
  "type": "learning",
  "text": "prefers tabs over spaces",
  "kind": "preference",
  "confidence": 0.8,
  "evidence": "said so in review 12",
  "meta": { "source": { "kind": "agent" }, "tags": ["style"] }
}
```

### Methods

| Method | Behaviour |
| --- | --- |
| `StoreItem::document(text, meta)` | A text document with no title or MIME. |
| `StoreItem::learning(text, kind, confidence, meta)` | A learning with no evidence. |
| `kind() -> ItemKind` | `Document`, `Conversation` or `Learning` (`ItemKind::ALL`, `as_str`). |
| `confidence() -> Option<f32>` | A learning's confidence; `None` otherwise. |
| `meta()` / `meta_mut()` | The metadata. |
| `render_text() -> String` | A document: `# {title}\n\n{body}` when the title is non-blank, else the body. A conversation: one `Turn::render()` line per turn, joined by `\n`. A learning: its text. This is what `Hit::text` carries. |
| `validate() -> Result<()>` | See below. |
| `fingerprint() -> String` | See below. |

`Turn::render()` is `role: text`, followed by ` [tools: name (id), name]` when
the turn made tool calls (the id only when one was assigned), so tool calls
stay searchable in fetch and list results.

### Validation

`StoreItem::validate` fails with `Error::InvalidRequest` for:

- a document whose body is blank text (`document body must not be empty`) or
  an unresolved `Uri`;
- a conversation with no turns, or any turn whose text is blank;
- a learning whose text is blank, or whose confidence is outside `0.0..=1.0`
  (NaN included, as it is not in the range).

Nothing else is checked: metadata is not validated beyond what its types
enforce (`Namespace` is checked when parsed).

### Fingerprint

`fingerprint()` is a stable 40-character lowercase hex string: the first 20
bytes of the SHA-256 of the item's JSON serialisation, with
`meta.observed_at`, `meta.observed_actor` and `meta.tool_call.id` cleared
first. For conversations, `meta.channel` is cleared too: channel records
where a turn was observed, while its thread and turn positions define its
identity. The fingerprint covers **everything else**: kind, text, title,
turns, learning kind, confidence, evidence, and every metadata field including
`namespace` (a root namespace is not serialised, so root items hash as they did
before namespaces existed).

`observed_at` is excluded because it records *when* the item was seen, not
*what* it is. A host stamps it on every store; hashing it would turn a retried
learning, or an unchanged file re-synced, into a new item each time.
`observed_actor` says *who* said it, which an engine sends only with
attribution on; hashing it would store the same email again once that is
turned on.

`meta.tool_call.id` identifies a provider invocation, so a new call that retries
the same learning must not create a duplicate. The tool name still participates
in identity, and the stored metadata retains the original invocation id. Tool
calls inside conversation turns remain part of the conversation's content.

Two items with the same fingerprint are the same item. Engines derive
idempotency from it: the reference engine uses the fingerprint as the item id.
See [operations.md](operations.md#idempotency-and-fingerprints).

## `ItemId`

A transparent newtype over `String` (a bare JSON string), assigned by the
engine and opaque to the host. `ItemId::new`, `as_str`, `Display`, and `From`
for `&str` and `String`.

## `StoreReceipt`

`{ id: ItemId, replayed: bool }`. `replayed` is `true` when the engine already
held this exact item and wrote nothing.

## `MemoryMeta`

Where an item came from and what it is about. `#[serde(default)]`, so every
field may be omitted on the wire; unset options, an empty `tags` and a root
namespace are omitted on serialisation.

| Field | Type | Meaning |
| --- | --- | --- |
| `namespace` | `Namespace` | The memory node the item lives at; the root by default. See [namespaces.md](namespaces.md). |
| `workspace` | `Option<String>` | Absolute path or logical workspace id. |
| `folder` | `Option<String>` | Containing folder, absolute or workspace-relative. |
| `file_path` | `Option<String>` | The file the item was read from. |
| `language` | `Option<String>` | Code language (`rust`) or natural-language tag (`en`). |
| `repo` | `Option<String>` | `owner/name` or a remote URL. |
| `commit` | `Option<String>` | Commit the item was read at. |
| `url` | `Option<String>` | URL the item was read from. |
| `thread_id` | `Option<String>` | Conversation thread. |
| `channel` | `Option<String>` | Channel the conversation or message came from, such as `telegram`. |
| `turns` | `Option<TurnRange>` | `{ first, last }`, zero-based and inclusive. |
| `agent_id` | `Option<String>` | Agent that produced the item. |
| `tool_call` | `Option<ToolCallRef>` | `{ name, id? }` of the producing tool call. |
| `source` | `SourceRef` | `{ kind: SourceKind, id? }`; always present, defaults to kind `agent`. |
| `tags` | `Vec<String>` | Free-form tags. |
| `observed_at` | `Option<DateTime<Utc>>` | When the underlying fact was observed, as opposed to stored. Excluded from the fingerprint. |
| `observed_actor` | `Option<ObservedActor>` | `{ id, name? }`: who said or did it when not the memory's owner, such as an email's sender or a channel's (`id` is `type:id`, `user:priya@acme.com`, `user:+15551234567`). Excluded from the fingerprint; the CortexDB engine sends it only with attribution on. |

`MemoryMeta::from_source(kind, id)` sets only the source. `MemoryMeta::overlay(host)`
uses set host optional fields in preference to caller values; the host's
namespace and source always replace the caller's, including root/default
values, and host tags append uniquely after caller tags.
`SourceKind` is
`folder | file | link | github | rss | conversation | agent |
import` (`SourceKind::ALL`, `as_str`); `agent` is the default.
`ObservedActor::for_channel_sender`
normalizes phone senders on `whatsapp`, `signal`, `imessage`, `linq` and `sms`,
emails on any channel, and otherwise uses `<channel>:<sender>`. That channel
vocabulary lives in TinyMemory to avoid a dependency on a host channel crate.

## `MetaFilter`

Selects items for recall, fetch, list, explore and forget-by-filter. It is
`#[serde(default)]` and omits unset fields, so `{}` is the empty filter.

Matching rules (`MetaFilter::matches(kind, &meta)`): **every set field must
match**; an empty filter matches everything.

| Field | Type | Rule |
| --- | --- | --- |
| `reach` | `Option<Reach>` | Item's namespace must be in reach; `None` admits every namespace. |
| `workspace`, `language`, `repo`, `commit`, `url`, `thread_id`, `channel`, `agent_id` | `Option<String>` | Exact match. An item with no value never matches a set field. |
| `folder`, `file_path` | `Option<String>` | Exact, or a path prefix on a `/` boundary: `/a/b` matches `/a/b` and `/a/b/c.rs`, not `/a/bc`. A trailing `/` on the filter value is ignored. |
| `turns` | `Option<TurnRange>` | Exact range. |
| `tool_call` | `Option<String>` | Exact tool name of the item's `tool_call`. |
| `source_id` | `Option<String>` | Exact `meta.source.id`. |
| `kinds` | `Vec<ItemKind>` | Item kind is in the list; empty means all. |
| `sources` | `Vec<SourceKind>` | `meta.source.kind` is in the list; empty means all. |
| `tags_any` | `Vec<String>` | Item has at least one of these tags; empty means no constraint. |
| `observed_after` | `Option<DateTime<Utc>>` | `observed_at >= observed_after` (inclusive). |
| `observed_before` | `Option<DateTime<Utc>>` | `observed_at < observed_before` (exclusive). |

An item with no `observed_at` never matches when either bound is set.

Helpers: `MetaFilter::kinds(iter)`, `is_empty()` (true exactly when the filter
equals the default, so a filter holding only a `reach` is **not** empty),
`admits_kind(kind)`.

```json
{
  "reach": { "at": "team:acme/agent:writer", "inherit": true, "descendants": false },
  "kinds": ["document", "learning"],
  "sources": ["folder", "github"],
  "folder": "/notes/rust",
  "tags_any": ["style", "review"],
  "observed_after": "2026-01-01T00:00:00Z",
  "observed_before": "2026-10-01T00:00:00Z"
}
```

### Matching example

```rust
use tinymemory_api::{ItemKind, MemoryMeta, MetaFilter, SourceKind};

let mut meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
meta.file_path = Some("/notes/rust/ownership.md".into());

let under = MetaFilter { file_path: Some("/notes/rust".into()), ..MetaFilter::default() };
let lookalike = MetaFilter { file_path: Some("/notes/ru".into()), ..MetaFilter::default() };
assert!(under.matches(ItemKind::Document, &meta));
assert!(!lookalike.matches(ItemKind::Document, &meta)); // not on a `/` boundary
```
