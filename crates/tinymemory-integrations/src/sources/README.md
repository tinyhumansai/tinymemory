# sources

`tinymemory_integrations::sources` (features `sources` and `sources-network`):
readers that turn a source into `StoreItem`s — a folder, a single file, a web
page, a GitHub repository, an RSS feed, or the
host's local conversation threads. Conversion to markdown and language
detection come from the sibling [`documents`](../documents/README.md) module.
Architecture overview:
[`docs/architecture/integrations.md`](../../../../docs/architecture/integrations.md).

The host still owns persistence and migration of its configuration. This
module provides the reusable configured-source wire types
(`MemorySourceConfig`, `MemorySourceKind`), validation and layout rules
(`SourceSpec`), restart-safe sync state types, and the source reading and
placement mechanics. The host supplies the filesystem state store, placement
callback, and job enqueue port.

## Layers

| Module | Owns |
| --- | --- |
| `types` | reader entries keyed by `SourceKind`, the host `MemorySourceConfig` and `MemorySourceKind` wire types, and reader output types (`SourceItem`, `SourceContent`, `ContentType`) |
| `spec` | host source target normalization, duplicate/schedule/namespace validation, and per-source `MemoryLayout` selection |
| `state` | serializable `SourceState` and `SourceStatus`, plus interrupted-sync recovery; it owns no storage |
| `readers` | `SourceReader` (list, read, read as a `StoreItem`) and one reader per kind, each in its own module directory; `local_file` holds the shared size-capped read and the path-containment guard |
| `fetch` | one URL into a `RawDocument` or a link item (`sources-network`); the RSS and web-page readers fetch through it with their own body caps |
| `fetch::ssrf` | the SSRF guard: scheme and host policy, one address classifier for literal and resolved addresses, a public-only DNS resolver, per-hop redirect checks, and a capped body reader |
| `items` | reader output to `StoreItem`s with `MemoryMeta` filled per kind; `collect_items` drives a reader end to end |
| `sync` | generic source sync loop and batch storage; `store_placed` applies a host placement callback and returns touched-node belief jobs |
| `error` | the module `Error`, mapped onto `tinymemory_api::Error` |

## Kinds and metadata

Every item's `meta.source` is `SourceRef { kind, id: Some(entry.id) }`.

| Config kind | `SourceKind` | Item | Metadata |
| --- | --- | --- | --- |
| `folder` | `Folder` | document | `workspace`, `folder` (containing directory), `file_path`, `language`, `observed_at` (mtime), `mime` |
| `file` | `File` | document | as `folder`; `file_item` reads a path with no configured source |
| `web_page` | `Link` | document | `url` |
| `github_repo` | `Github` | document | `repo` (`owner/name`), `commit` (commits), `url` (issues, PRs), `observed_at` |
| `rss_feed` | `Rss` | document | `url` (the entry's link), `observed_at` (published) |
| `conversation` | `Conversation` | conversation | `workspace`, `thread_id`, `turns`, `observed_at` (last turn) |

## Folder selection

With a glob, a folder source takes exactly the matching files. Without one it
takes markdown, plain text and source code (`is_default_candidate`). Either way
it skips hidden files and directories and `target`, `node_modules`,
`__pycache__` and `venv`, never follows symlinks while walking, refuses files
over `FOLDER_FILE_SIZE_CAP_BYTES` (10 MiB), and confines reads to the folder
root (`readers::local_file::ensure_within_base`). A relative path is anchored on the workspace, not
the process working directory.

## Who decides when

`readers::reader_for` hands out only the local readers (folder, file,
conversation), which are safe to drive on a timer. Network readers are
constructed explicitly, or through `reader_for_request` (feature
`sources-network`) for an explicit user request. Scheduling, credentials, OAuth and egress budgets stay with the host.

## Fetching

Every network fetch of a user-configured URL goes through `fetch` and its
SSRF guard. A hostname is checked as text (private and reserved IP literals,
`localhost`, `.local`/`.internal`, single-label names), its resolved addresses
are checked again by the client's resolver, which pins the connection to an
address it has vetted, and every redirect hop is re-checked. Bodies are read
against a cap while streaming: 32 MiB for `fetch_url`, 10 MiB for a web page,
5 MiB for a feed. The client sends the user agent `openhuman`, times out after
20 seconds, and allows only `http(s)`. An IPv6 literal URL is always refused,
even a public address (the bracketed host fails the IP parse and falls into the
single-label rule): fail closed. Failures are typed — `Invalid` for a refused or malformed
URL, `Unreachable`, `Upstream` for a failure status, `TooLarge`.

Page titles and feed text are decoded with the `documents::html` helpers, so
named and numeric entities decode the same way everywhere.

## Features

- `sources` — the local readers, `items` and `types`; implies
  `documents`. Links no HTTP client. JSON Schema derives are opt-in through
  the separate `schema` feature.
- `sources-network` — adds the GitHub, RSS and web-page readers, `fetch`, and
  the SSRF guard.
- `schema` — enables `schemars` derives for source config and reader types;
  downstream consumers that generate schemas must enable it explicitly.
