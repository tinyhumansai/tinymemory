# Integrations

`tinymemory-integrations` holds everything that connects the contract
([`tinymemory-api`](api.md)) to the outside world. Each integration is a module
behind a Cargo feature, so a host links only what it uses. This page covers the
module map, documents, safety and the legacy import. Sources are large enough
for their own page: [integrations-sources.md](integrations-sources.md). The
CortexDB engine and the registry are in [cortex.md](cortex.md).

| Module | Feature | Page |
| --- | --- | --- |
| `cortex`, `registry`, `config` | `cortex` (default) | [cortex.md](cortex.md) |
| `documents` | `documents`, `documents-office` | [Documents](#documents) |
| `sources` | `sources`, `sources-network` | [integrations-sources.md](integrations-sources.md) |
| `safety` | `safety` | [Safety](#safety) |
| `import` | `legacy-import` | [Legacy v1 import](#legacy-v1-import) |

Per-feature dependency weight is in the
[crate README](../../crates/tinymemory-integrations/README.md). Each module also
has its own README with the full detail:
[`documents`](../../crates/tinymemory-integrations/src/documents/README.md),
[`sources`](../../crates/tinymemory-integrations/src/sources/README.md),
[`safety`](../../crates/tinymemory-integrations/src/safety/README.md) and
[`import`](../../crates/tinymemory-integrations/src/import/README.md).

## How the pieces compose

Sources and converters do not schedule or call engines. The host chooses those
steps; the registry's `EngineBinder` always places safety and timing around a
configured, host-installed or transient engine:

```text
sources ──▶ documents ──▶ EngineBinder ──▶ TimedEngine ──▶ ScrubbingEngine ──▶ engine
```

`sources` depends on `documents` for conversion (the `sources` feature implies
`documents`). The `cortex` feature implies `safety` so every `EngineBinder` path
has the scrubber available. Scheduling, credential resolution, OAuth and
egress budgets remain the host's.

## Errors

The crate's `Error` is `tinymemory_api::Error`. `documents`, `sources` and
`import` keep a typed error each, because their failures are worth matching on
before they reach an engine, and each converts into the contract error with
`From`:

| Module error | Maps to |
| --- | --- |
| `documents::Error::Invalid`, `TooLarge` | `InvalidRequest` |
| `documents::Error::UnsupportedFormat` | `Unsupported` |
| `documents::Error::Converter` | `Engine` |
| `sources::Error::Invalid`, `PathEscape`, `TooLarge`, `Json` | `InvalidRequest` |
| `sources::Error::NotFound` | `NotFound` |
| `sources::Error::Unreachable` | `Unavailable` |
| `sources::Error::Upstream`, `Reader`, `Io` | `Engine` |
| `sources::Error::Document(e)` | whatever `e` maps to |

## Documents

Feature `documents` (and `documents-office`). The module turns bytes into
markdown and wraps the result as a `StoreItem::Document`. It does no I/O.

### Format detection

`DocumentFormat::sniff(bytes, filename, mime)` consults three signals in order
of trustworthiness:

1. **Magic bytes.** `%PDF-` is a PDF. A zip (`PK\x03\x04`) is an Office package
   of some kind, refined by its part names read from the central directory
   (`word/`, `xl/`, `ppt/`); failing that, a MIME type or filename naming an
   Office format; failing that, `Docx`.
2. **The declared MIME type**, parameters stripped and compared
   case-insensitively. Legacy binary types (`application/msword`,
   `application/vnd.ms-excel`, `application/vnd.ms-powerpoint`) are deliberately
   not claimed.
3. **The filename.** A name `language_for_path` recognises is `Code` (checked
   first, so `CMakeLists.txt` is code, not plain text); otherwise the extension
   (`md`, `txt`, `html`, `pdf`, `docx`, `xlsx`/`xlsm`, `pptx`).

With none of those, a buffer opening with `<!doctype html` or `<html` is HTML,
a NUL-free valid UTF-8 buffer is plain text, and anything else is `Unknown`.
The formats are `Markdown`, `PlainText`, `Html`, `Code`, `Pdf`, `Docx`, `Xlsx`,
`Pptx` and `Unknown`; each has a canonical `mime()` and `extension()`, and
`is_textual()` marks the four a converter can decode as text.

`language_for_path(path)` returns a stable lowercase language name (`rust`,
`python`, `typescript`, `dockerfile`, ...) from a well-known file name or an
extension, or `None`. The names are a wire contract: they land in
`MemoryMeta::language`.

### Conversion

| Type | Role |
| --- | --- |
| `RawDocument` | bytes plus optional filename, declared MIME and origin (a URL) |
| `ConvertedDocument` | markdown (never empty), title, format, language, source size, converter metadata |
| `DocumentConverter` | the object-safe async seam: `name`, `supports(format)`, `convert(&RawDocument)` |
| `NativeConverter` | markdown, plain text, HTML and code, with no dependencies |
| `OfficeConverter` | PDF, DOCX and PPTX (feature `documents-office`); XLSX remains recognized but is temporarily unsupported |
| `ConverterChain` | converters in priority order; the first that claims the format converts |
| `check_size` | rejects an empty body or one over `MAX_DOCUMENT_BYTES` |
| `markdown_from_text` | the synchronous core, for callers already holding text |

`NativeConverter` stores markdown, plain text and code exactly as written
(rewriting them would change the user's words or the program's meaning); HTML
goes through the structural `html::to_markdown`, and `html::extract_title`
supplies its title. `ConverterChain::default()` holds only `NativeConverter`;
`prepend` and `push` add converters. The chain does not fall through on
failure: a converter that claims a format and fails has found a real problem,
and retrying it elsewhere would turn a precise error into a vague one. A format
no converter claims is `Error::UnsupportedFormat`, naming the format and
listing what the build can convert, never a silent empty document.

`MAX_DOCUMENT_BYTES` is 32 MiB, checked on the raw bytes before conversion.

### OfficeConverter

Under `documents-office`, `OfficeConverter` is pure Rust (`pdf-extract`, `zip` +
`quick-xml`):

| Format | Reader | Markdown |
| --- | --- | --- |
| PDF | text layer only (a scanned PDF is refused as having no text) | page text, whitespace-normalised |
| DOCX | `word/document.xml` | one paragraph per `w:p` |
| PPTX | `ppt/slides/slideN.xml` | slides in numeric order |
| XLSX | temporarily disabled while awaiting a safe Calamine release | `Error::UnsupportedFormat`; the workbook is not opened |

It refuses hostile input: an archive whose declared uncompressed size exceeds
`MAX_DECOMPRESSED_BYTES` (64 MiB), each entry read being capped as well. XLSX
and XLSM are recognized by format sniffing but are not claimed by the built-in
converter; a host may bind its own `DocumentConverter`. CSV and plaintext stay
on the native text converter. A `pdf-extract` panic on a malformed file is
caught and reported as an unreadable document. Parsing is CPU-bound and runs inline in the async
`convert`; a host on a shared executor calls `convert_blocking` from its own
blocking pool. A host typically prepends it:

```rust,ignore
let chain = ConverterChain::default().prepend(Box::new(OfficeConverter));
```

### Items

`document_item(converter, &raw, meta)` converts and wraps in one call;
`converted_item(converted, &raw, meta)` wraps an existing conversion. The item
is a `StoreItem::Document` with the markdown as `DocumentBody::Text`, the
format's canonical MIME type as `mime`, and the caller's `MemoryMeta`. The
caller owns the metadata; the only field filled in is `language` (from the
conversion, else the file extension), and only when the caller left it unset.
The title is the converter's, else (for prose) the first markdown heading, else
the file name or origin. Code never takes a title from a heading, because a `#`
line in a script is a comment.

## Safety

Feature `safety`. `scrub_item(item)` removes credentials and personal
identifiers from every free text a `StoreItem` carries, returning a
`Sanitized<StoreItem>` whose report tallies what changed. It runs on-device with
regular expressions and checksums, makes no network calls, never fails, and
errs toward redacting a harmless string rather than letting a secret into a
long-lived store. `ScrubbingEngine` applies it to every write method. The
registry's `EngineBinder` composes that decorator non-optionally for every
engine it returns.

What it does, in order, on a text:

1. blocks private-key blocks whole (`[REDACTED_PRIVATE_KEY]`);
2. redacts credential markers: the value after `/secret/` in a one-time-secret
   URL and after a `Bearer ` scheme;
3. redacts credential shapes: provider token prefixes, JWTs and `key=value`
   assignments with a sensitive key;
4. redacts PII with typed tokens (`[REDACTED_PII_CPF]`, ...): checksum-gated
   national IDs, credit cards, IBANs and phone numbers.

Per kind, `scrub_item` scrubs a document's title and text body, every
conversation turn's text, a learning's text and evidence, and `meta.url`
(query strings carry tokens). Metadata identifiers (paths, repo, commit,
thread, agent ids) and `DocumentBody::Uri` bodies are left alone, because
filters match on the identifiers. The single tunable is `Policy`'s
`BareCardGate` (`LuhnOnly`, the default and strictest, or `Corroborated`).
JSON values are scrubbed with `sanitize_json`, which also redacts by key name.
Email addresses are detected (`has_likely_email`) but not redacted. See
[`safety/README.md`](../../crates/tinymemory-integrations/src/safety/README.md)
for the pipeline, the strict `has_likely_pii` boundary check and the known
limits.

## Legacy v1 import

Feature `legacy-import`. The `import` module reads a v1 (embedded TinyCortex)
workspace and yields v2 `StoreItem`s, resumably. The v1 engine is not linked:
the importer reads its SQLite files with `rusqlite` (bundled), opened
read-only, plus chunk bodies from disk. It never writes to the legacy
workspace.

### Detection

`LegacyWorkspace::open(path)` accepts a workspace with `<path>/memory/memory.db`
(the first v1 engine), `<path>/memory_tree/chunks.db` (the later TinyCortex
engine, which wrote no `memory.db`), or both. A `memory.db` must be a SQLite
database with the `memory_docs`, `episodic_log` and `user_profile` tables and
the columns the importer reads. A missing path is `Error::NotFound`; anything
else that is not a v1 store is `Error::NotLegacy` with the reason. Columns added
by later v1 migrations are probed and used when present. Beside a `memory.db`,
the chunk store is optional and skipped when absent, not SQLite, or without a
usable `mem_tree_chunks`; one that exists but cannot be read is an error.
`LegacyWorkspace::counts()` sizes a store per section without importing it,
exactly as `items()` would yield it. Per-profile stores (the suffixed pairs `memory-1/memory.db` and
`memory_tree-1/chunks.db`) are listed by `LegacyWorkspace::store_suffixes` and
opened with `LegacyWorkspace::open_store`; their items carry legacy ids
prefixed with the store directory and the tag `store:memory-1`.

### Mapping v1 to v2

Every item gets `meta.source = { kind: Import, id: <legacy id> }` and
`meta.workspace` set to the workspace path.

| Section (in order) | Legacy rows | Legacy id | v2 item |
| --- | --- | --- | --- |
| documents | `memory_docs` in document namespaces | `memory_docs:<id>` | `Document` |
| chunks | `mem_tree_chunks` grouped by source | `mem_tree_chunks:<kind>:<id>` | `Conversation` for `chat`, else `Document` |
| conversations | `episodic_log` grouped by session | `episodic_log:<session>` | `Conversation` |
| learnings | `memory_docs` in `learning:*` and `global` | `memory_docs:<id>` | `Learning` |
| profile | live `user_profile` facets | `user_profile:<id>` | `Learning(Preference)` |
| events | `event_log` (extracted facts, decisions, commitments, …) | `event_log:<id>` | `Learning` |
| lessons | `episodic_log` turns with a `lesson` | `episodic_log:lesson:<id>` | `Learning(Other)` tagged `lesson` |
| graph | `graph_global` and `graph_namespace` relations | `graph_global:<rowid>`, `graph_namespace:<rowid>` | `Learning(Fact)` "subject predicate object" |
| files | `MEMORY_GOALS.md`, `persona/directives.md` | `file:<path>` | `Learning(Other)` tagged `goals` / `persona` |

Notable decisions: `event` namespaces and the `kv_*` tables are not imported
(bookkeeping, not recall material); unknown conversation roles become `User`;
learning classes map onto kinds (`style`/`channel` to `Preference`, `identity`
to `Fact`, `tooling` to `Procedure`, `veto` to `Correction`, others to
`Other`); dropped or forgotten profile facets and blank rows are skipped. The
full rules are in
[`import/README.md`](../../crates/tinymemory-integrations/src/import/README.md).

### Checkpoint and resumption

Sections run in a fixed order and, within one, keys ascend in SQLite `TEXT`
order (lessons and graph rows by integer id). New sections are appended to the order, so
a checkpoint persisted before one existed resumes into it. A `Checkpoint` records the last yielded key per section; every
`ImportedItem { item, checkpoint }` carries the checkpoint covering it and
everything before. `LegacyWorkspace::items_from(&checkpoint)` yields exactly
what `items()` yields after that item (given the legacy store did not change
in between). A checkpoint serialises with `to_json` and `from_json` for the
host to persist. Pages of keys are fetched `DEFAULT_PAGE_SIZE` (256) at a time,
so memory is bounded.

### migrate and migrate_with

```rust,ignore
let workspace = LegacyWorkspace::open(path)?;
let from = saved.map(|json| Checkpoint::from_json(&json)).transpose()?;
let report = migrate_with(engine.as_ref(), workspace, from, |checkpoint| {
    save(checkpoint.to_json());
})
.await?;
```

`migrate(engine, workspace, from)` stores every item after `from` (all of them
for `None`) in `store_many` batches of at most `MAX_STORE_MANY` (100) and
returns a `MigrationReport { stored, replayed, batches, checkpoint }`.
`migrate_with` also calls `on_batch(&Checkpoint)` after each stored batch.

Resume semantics:

- After a batch is stored, its last item's checkpoint is committed: passed to
  `on_batch` and kept as `report.checkpoint`.
- An engine failure is `Error::Engine { source, checkpoint }`, carrying the last
  committed checkpoint (or `from` if no batch was stored). Call `migrate` again
  with it. The failed batch may have stored a prefix; the engine answers those
  as replays, so a resumed or repeated run never duplicates (a second full run
  reports every item as `replayed`).
- A legacy read failure (`Sqlite`, `Io`) is returned as is. Every checkpoint
  committed before it has already reached `on_batch`.
- The workspace is taken by value: its SQLite handle is not `Sync`, and owning
  it keeps the future `Send` so a long import can run on a spawned task.

## Engine and registry

For `CortexEngine`, `list_engines`, `build_engine`, `EngineCredential` and
`MemoryConfig`, see [cortex.md](cortex.md).
