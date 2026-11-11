# import

The `import` module of `tinymemory-integrations` (feature `legacy-import`):
reads a legacy (v1, embedded TinyCortex) workspace and yields TinyMemory v2
`StoreItem`s, resumably. The v1 engine that wrote the store is not linked: the
importer reads its SQLite files directly with `rusqlite`, opened read-only, and
chunk bodies with `std::fs`. It never writes to the legacy workspace.

The module has no sub-features: being the legacy reader is its whole job. It
needs only `rusqlite` (bundled SQLite), `serde`, `serde_json` and `thiserror`.
Architecture overview:
[`docs/architecture/integrations.md`](../../../../docs/architecture/integrations.md).

## Surface

| Item | Purpose |
| --- | --- |
| `LegacyWorkspace::open(path)` | Detects a v1 store or refuses with a typed error. |
| `LegacyWorkspace::store_suffixes(path)` / `open_store(path, suffix)` / `store_suffix()` | Lists and opens the per-profile stores (`memory-1`, `memory_tree-1`, …). |
| `LegacyWorkspace::counts()` | `LegacyCounts` per section, exactly what `items()` yields: one aggregate query per `memory.db` section, the chunk store through the chunk reader, no item decoded; `total()`, `is_empty()`. Non-exhaustive. |
| `LegacyWorkspace::skip_connector_syncs(bool)` | Opt in to leaving out everything v1 synced from connectors (Composio and the connector path); see [Connector syncs](#connector-syncs). Off by default. |
| `LegacyWorkspace::has_memory_db()` / `has_chunks()` | Which of the two v1 databases the workspace has. |
| `LegacyWorkspace::items()` / `items_from(&Checkpoint)` | Streams `Result<ImportedItem>` from the start or after a checkpoint. |
| `Items::with_page_size(n)` | Keys fetched per query (default `DEFAULT_PAGE_SIZE`, 256). Does not affect output. |
| `ImportedItem { item, checkpoint }` | An item and the checkpoint to persist once it is stored. |
| `Checkpoint` | Last yielded key per section; `to_json` / `from_json` for the host to persist. |
| `migrate(engine, workspace, from)` | Copies every item after `from` into a `MemoryEngine`, in `store_many` batches; returns a `MigrationReport`. |
| `migrate_with(engine, workspace, from, on_batch)` | `migrate`, calling `on_batch(&Checkpoint)` after each stored batch so the host can persist it. |
| `MigrationReport { stored, replayed, batches, checkpoint }` | What a run did, and where to resume. |
| `Importer::resume_plan(file, billing_ok, paused)` | Pure decision for whether a background tick resumes an import, resumes a failed-item retry, or waits. |
| `record_resume_failure(store, error)` | Persists an automatic resume failure using only the host checkpoint store; no engine or pacer is needed. |
| `Error` / `Result` | `NotFound`, `NotLegacy`, `Sqlite`, `Io`, `Json`, `Engine { source, checkpoint }`; `Error::checkpoint()` reads the resume point. |

## Detection

A v1 store is either of two databases, and a workspace may hold both. The
first v1 engine wrote `<path>/memory/memory.db`; the later one (TinyCortex)
wrote only `<path>/memory_tree/chunks.db`, so a store with just the chunk
store is a v1 store too.

`open(path)` accepts a workspace with either. When `memory/memory.db` exists
it must be a SQLite database with the `memory_docs`, `episodic_log` and
`user_profile` tables and the columns the importer reads. A missing path is
`NotFound`; anything else that is not a v1 store (a file, neither database, a
`memory.db` that is not SQLite or has a different schema) is `NotLegacy` with
the reason. Columns that later v1 migrations added are probed
with `pragma_table_info` and used when present: `memory_docs.logical_namespace`,
`memory_docs.taint`,
`episodic_log.tool_calls_json`, `user_profile.state` / `user_state` / `class`
/ `evidence_refs_json`, `mem_tree_chunks.content_path` and `mem_tree_chunks.owner`.

Beside a `memory.db`, `memory_tree/chunks.db` is optional: if it is absent,
not SQLite, or has no usable `mem_tree_chunks` table, the chunk section is
skipped silently. Without a `memory.db` it is the store, and an unusable one
is `NotLegacy`. A chunk store that exists but cannot be read (permissions, a
failing disk) is an error, not skipped: it may hold the user's data, and a
migration must not report itself complete without it.

`counts()` sizes a store without importing it, so a host can tell whether
there is anything to import and show progress against a total. Every section
counts with the very predicate its scan filters by (a SQL function over Rust's
`str::trim`, and for the chunk store the chunk reader itself, which reads
bodies from their files), so the counts are exactly what `items()` yields,
with or without `skip_connector_syncs`.

### Per-profile stores

v1 kept a profile with dedicated memory in a suffixed pair beside the main
store: `memory<suffix>/memory.db` and `memory_tree<suffix>/chunks.db`, the
suffix being `-1`, `-2`, …. `LegacyWorkspace::store_suffixes(path)` lists
them (every `memory-*` or `memory_tree-*` directory with a valid suffix), and
`LegacyWorkspace::open_store(path, suffix)` opens one with the same rules as
`open` (which is `open_store(path, "")`). Its items carry legacy ids
prefixed with the store directory (`memory-1/memory_docs:<id>`), so they never
collide with the main store's, and the tag `store:memory-1`. The workspace
files belong to the main store only. A host imports the main store and each
profile store with a checkpoint of its own.

## Mapping

Every item gets `meta.source = { kind: Import, id: <legacy id> }` and
`meta.workspace = <canonical workspace path>`.

| Section (in order) | Legacy rows | Key / legacy id | v2 item |
| --- | --- | --- | --- |
| documents | `memory_docs` in document namespaces | `document_id` / `memory_docs:<id>` | `Document` |
| chunks | `mem_tree_chunks` grouped by `(source_kind, source_id)` | the pair / `mem_tree_chunks:<kind>:<id>` | `Conversation` for `chat`, else `Document` |
| conversations | `episodic_log` grouped by `session_id` | `session_id` / `episodic_log:<id>` | `Conversation` |
| learnings | `memory_docs` in `learning:*` and `global` | `document_id` / `memory_docs:<id>` | `Learning` |
| profile | live `user_profile` facets | `facet_id` / `user_profile:<id>` | `Learning(Preference)` |
| events | `event_log` rows | `event_id` / `event_log:<id>` | `Learning` |
| lessons | `episodic_log` turns with a `lesson` | `id` / `episodic_log:lesson:<id>` | `Learning(Other)` |
| graph | `graph_global`, then `graph_namespace` relations | `rowid` / `graph_global:<rowid>`, `graph_namespace:<rowid>` | `Learning(Fact)` |
| files | `MEMORY_GOALS.md`, `persona/directives.md` | name / `file:<path>` | `Learning(Other)` |

### `memory_docs` namespaces

A row's logical namespace is `logical_namespace` when that column exists and
is set, else `namespace` with the sanitiser undone for the known v1 section
prefixes (`learning_style` → `learning:style`; only the first `_` can be
restored). Then:

- `learning:<class>` or `learning` → learnings section;
- `global` → learnings section;
- `event` / `event:*` → **skipped**. These are raw event payloads the v1
  engine kept for bookkeeping; what they meant already lives in the episodic
  log and in the learnings distilled from them, and as JSON blobs they would
  only add noise to recall;
- anything else (`document:*`, `source:*`, `conversation:*`, custom
  `Memory::store` namespaces) → documents section.

Rows with blank content are skipped in every section.

### Taint

v1 stamped every `memory_docs` row with a `taint`: `internal` for what the
user and the agent wrote, `external_sync` for content synced from an outside
service (Gmail, Slack, Notion, Composio, MCP, ...), and kept tainted content
out of decisions to call external-effect tools. v2 has no taint field, so an
item from a row whose `taint` is anything but `internal` gets the tag
`taint:external_sync` (`EXTERNAL_SYNC_TAG`), in every section a `memory_docs`
row can land in (documents, learnings, `global`). The tag rides in the item's
metadata, which a CortexDB engine stores whole, so the host can read it back on
recall. The decode fails closed like v1's: an unknown or empty value is
external. A store from before the `taint` column is read as all `internal`,
which is how v1 read it. `episodic_log`, `user_profile` and the chunk store
have no taint in v1 and get no tag.

### Documents

`title` from `title` (none when blank), body = `content`, `tags` = the strings
in `tags_json` plus `ns:<logical namespace>`, `observed_at` = `updated_at`,
`url` and `mime` from `metadata_json` when present.

### Chunks

Chunks of one source are ordered by `(seq_in_source, id)`. A chunk's text is
the file `memory_tree/content/<content_path>` when the column is set, the path
is a plain relative path, and the file exists; otherwise the stored preview.
A `chat` source becomes a conversation of one `User` turn per chunk (chat
chunks are transcripts of host channels, whose speakers are people, and, from
Composio's Slack sync, channel messages under `slack:{conn}`), with
`thread_id` = `source_id` and `turns` = `0..=n-1`. Every other kind becomes a
document whose body is the chunks joined by blank lines. Tags are the union of
the chunks' `tags_json` plus `source_kind:<kind>`; `observed_at` is the latest
chunk timestamp. A chunk source may duplicate a `memory_docs` document; the
two carry different legacy ids, so an engine stores both.

### Conversations

Turns are ordered by `(timestamp, id)`; blank turns are dropped and a thread
with none left is skipped. Roles map case-insensitively: `user`/`human` →
`User`; `assistant`/`ai`/`agent`/`bot`/`model` → `Assistant`;
`system`/`developer` → `System`; `tool`/`function`/`tool_result` → `Tool`;
**anything else → `User`** (v1 itself wrote only `user` and `assistant`, so an
unknown role came from a host channel, whose speaker is a person). `at` =
`timestamp`; `tool_calls` from `tool_calls_json` (an array of calls, a single
call, or `{"tool_calls": [...]}`, naming the tool as `name`, `tool`,
`tool_name` or `function.name`), dropped when unparseable. `thread_id` =
`session_id`, `turns` = `0..=n-1`, `observed_at` = the last turn's time.
`lesson` is imported by the lessons section (below); `cost_microdollars` is
not imported.

### Learnings

A `learning:<class>` row's content is a JSON `LearningCandidate`. It becomes
`"<key>: <value>"` (a non-string value as compact JSON), `confidence` =
`initial_confidence` clamped to `0..=1` (0.5 when absent), `evidence` = the
`evidence` JSON, `observed_at` = the candidate's `observed_at` or else
`updated_at`, `tags` = `[class]`. The kind follows the class:

| Class | Kind |
| --- | --- |
| `style`, `channel` | `Preference` |
| `identity` | `Fact` |
| `tooling` | `Procedure` |
| `veto` | `Correction` |
| `goal`, unknown | `Other` |

Content that is not a candidate (not JSON, or no `key`/`value`) becomes
`Learning { kind: Other, confidence: 0.5, text: content }` tagged with the
namespace's class. A `global` row becomes
`Learning { kind: Fact, confidence: 0.5, text: content }` tagged `global`:
v1 kept always-relevant statements there.

`kv_global` and `kv_namespace` are **not imported**: v1 used them for engine
and host bookkeeping values, not for anything recall should surface.

### Profile

Facets with `state = 'dropped'` or `user_state = 'forgotten'` (when those
columns exist) or a blank value are skipped. The rest become
`Learning(Preference)` with text `"<key>: <value>"`, `confidence` from the
column (clamped), `evidence` = `evidence_refs_json`, `observed_at` =
`last_seen_at`, `tags` = `[facet_type, class]` (class when set).

### Events

v1 extracted typed atomic events from closed conversation segments into
`event_log`. Each event with text becomes a learning: `text` = `content`,
`confidence` from the column (clamped), `observed_at` = `created_at`,
`thread_id` = `session_id` (none when blank), `tags` = `event:<type>` and
`subject:<subject>` when set. The kind follows the type: `fact` and
`decision` → `Fact`, `preference` → `Preference`, `commitment`, `question`,
`foresight` and unknown → `Other`. A store without the table, or with one
missing any of `event_id`, `session_id`, `event_type`, `content`, `subject`,
`confidence`, `created_at`, has no events section.

### Lessons

The v1 archivist stored a `lesson` on an assistant turn. Each turn whose
lesson has text becomes `Learning { kind: Other, confidence: 0.5 }` with the
lesson as its text, tagged `lesson`, in the turn's thread, observed at the
turn's time. A store from before the column has none.

### Graph

v1 kept the entity relations it extracted as subject–predicate–object
triples, workspace-wide (`graph_global`) and per namespace (`graph_namespace`).
Each relation whose subject and object have text becomes
`Learning { kind: Fact, confidence: 0.5 }` with the text
`"<subject> <predicate> <object>"` (a blank predicate left out), tagged
`graph` and, for a namespaced one, `ns:<namespace>`, observed at `updated_at`.
Rows are walked by `rowid`, which is stable because the store is no longer
written. `attrs_json` (evidence counts) is not imported. A table that is
missing, or lacks `subject`, `predicate`, `object` or `updated_at`, is skipped.

### Files

The later v1 engine kept two markdown files beside its stores:
`MEMORY_GOALS.md` (long-term goals, at most about 2,000 characters) and
`persona/directives.md` (how the assistant should behave). Each that exists
and has text becomes one `Learning { kind: Other, confidence: 0.5 }` of its
whole trimmed text, tagged `goals` or `persona`, observed at the file's
modification time (none when the file system cannot report one). A missing file is skipped; one that cannot be read, or is
not UTF-8, is `Error::Io`. At most 256 KiB of a file is read: a longer one is
cut there, at a character boundary, and also tagged `truncated`.

## Connector syncs

v1 synced outside services (Gmail, Slack, Notion, Linear, GitHub, ClickUp, ...)
into its stores, through Composio and the older connector path. Those
connectors re-sync, so a host can leave them out with
`LegacyWorkspace::skip_connector_syncs(true)` (default `false`, which imports
everything). The skipped rows yield no item, so the checkpoint still advances
over them, and `counts()` excludes exactly the same rows. Conversations,
folder and file memory sources (`mem_src:*`), `conversations:agent`, meetings,
`global`, learnings, events, lessons, goals and persona files are never
skipped. The rules:

| Section | Skipped when |
| --- | --- |
| documents (`memory_docs`) | the logical namespace starts with `skill-` (Composio `SkillDoc` sync, `skill-{toolkit}`) or `source:` (connector path, `source:{toolkit}:{conn}`, stored as `source_...`; the resolver maps it back). Taint is not consulted: v1 also marked the agent's own `global` and flow notes `external_sync`. |
| chunks | `source_kind = 'email'`; or `source_id` starts with a toolkit prefix in `CONNECTOR_TOOLKIT_PREFIXES` (`gmail:`, `slack:`, `notion:`, `linear:`, `github:`, `clickup:`); or the store has an `owner` column and any chunk of the source has `owner LIKE '%-sync:%'` (`{toolkit}-sync:{conn}`) |
| profile | `facet_id` starts with `skill-` (Composio identity facets `skill-{toolkit}-{conn}-{kind}`) |
| graph | `graph_namespace.namespace` starts with `skill-`, `source:` or `source_`; `graph_global` is unaffected |

## Ordering and resumption

Sections run in the fixed order above; within a section keys ascend in SQLite
`TEXT` order (lessons and graph rows by integer id). Sections added later are appended to
the order, never inserted, so a checkpoint persisted by an older release
resumes into them. Each `ImportedItem` carries the checkpoint covering it and
everything before it. `items_from(&checkpoint)` yields exactly what `items()`
yields after that item, provided the legacy store did not change in between
(the cursor is a key, so a row inserted later below an already-passed key is
not seen). The iterator fetches one page of keys per query, so memory is
bounded by the page size and, for a conversation or chunk source, by that one
thread or source. After an error it yields nothing more; resume from the last
persisted checkpoint.

## Migrating into an engine

`migrate` is the whole backwards-compatibility path: open the v1 workspace,
hand it to `migrate` with the engine built from the host's config (CortexDB,
usually) and the checkpoint persisted by an earlier run, if any.

```rust,ignore
let workspace = LegacyWorkspace::open(path)?;
let from = saved.map(|json| Checkpoint::from_json(&json)).transpose()?;
let report = migrate_with(engine.as_ref(), workspace, from, |checkpoint| {
    save(checkpoint.to_json());
})
.await?;
```

- Items are read in the order above and sent in batches of at most
  `MAX_STORE_MANY` (100). After a batch is stored, its last item's checkpoint
  is *committed*: passed to `on_batch` and kept as `report.checkpoint`.
- An engine failure is `Error::Engine { source, checkpoint }`, with the last
  committed checkpoint (or `from`, if no batch was stored). Resume by calling
  `migrate` again with it. The failed batch may have stored a prefix of its
  items; the engine answers those as replays, so a resumed or repeated run
  never duplicates (a second full run reports every item as `replayed`).
- A legacy read failure (`Sqlite`, `Io`) is returned as is. Every checkpoint
  committed before it has been passed to `on_batch`; resuming from any of
  them, or from the start, only replays.
- The workspace is taken by value: its SQLite handle is not `Sync`, and owning
  it keeps the returned future `Send`, so a long import can run on a spawned
  task.

## Resumable driver: `Importer`

`Importer` runs the copy as a product does: progress and the resume
`Checkpoint` persist through a host `CheckpointStore` (an `ImportFile`, in the
JSON shape the product has always written), transient engine failures retry
with a `RetryPolicy` slept through the host's `Pacer`, items the engine
refuses are skipped and kept in `ImportFile::failed` for `begin_retry`, and
account-wide failures (credits, sign-in, unreachable engine) stop the run
without advancing the checkpoint past anything unstored. Consent and the
pause check are host callbacks. `begin`/`begin_retry` mark the run `Running`
(blocking, they read SQLite); `run`/`run_retry` drive it. The host owns which
runs are live, the state file's location and what follows a finished import.
