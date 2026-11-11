# documents

Document intake for TinyMemory, the `documents` module of
`tinymemory-integrations` (feature `documents`): work out what a file is, turn
it into markdown, and wrap it as the `StoreItem::Document` an engine stores.

This module does no I/O. Reading files and fetching URLs belongs to the
[`sources`](../sources/README.md) module, which depends on this one for
conversion and language detection. Architecture overview:
[`docs/architecture/integrations.md`](../../../../docs/architecture/integrations.md).

## Three decisions

**What the file is** is a detection problem with three unreliable signals.
`DocumentFormat::sniff` reads magic bytes first (the only signal a caller
cannot get wrong), then the declared MIME type, then the filename, then falls
back to looking at the bytes. A browser that sends
`application/octet-stream` for a PDF still gets a PDF. A filename
`language_for_path` recognises (`main.rs`, `Dockerfile`, `CMakeLists.txt`) is
`DocumentFormat::Code`; HTML stays HTML.

**What it becomes** is markdown. It survives every hop the content makes
afterwards: chunkers split on its headings, embedders read it as prose, and a
human can read the stored copy without a renderer. Plain text and code are
already valid markdown and are stored exactly as written; code is never
reflowed or run through the HTML converter, because that would change what it
says.

**What the item carries** is the caller's `MemoryMeta`. `document_item` fills
exactly one field, and only when the caller left it unset: `language`, from the
file extension. Provenance (source, workspace, URL, observation time) is the
caller's to state.

## Public surface

| Item | What it is |
| --- | --- |
| `DocumentFormat` | markdown / plain text / HTML / code / PDF / DOCX / XLSX / PPTX / unknown, and `sniff` |
| `language_for_path` | stable lowercase language name (`rust`, `python`, `typescript`) for a code file |
| `RawDocument` | bytes plus filename, declared MIME, and origin |
| `ConvertedDocument` | markdown plus title, source format, language, and converter metadata |
| `DocumentConverter` | the conversion seam — object-safe and async |
| `NativeConverter` | markdown, text, HTML and code, with no dependencies |
| `OfficeConverter` | PDF, DOCX, PPTX and XLSX, in-process (feature `documents-office`) |
| `ConverterChain` | converters in priority order; first claim wins |
| `document_item` / `converted_item` | the conversion wrapped as a `StoreItem::Document` |
| `markdown_from_text` | the synchronous core, for callers that already hold text |
| `html::to_markdown` | the structural HTML converter, usable on its own |
| `Error` / `Result` | the module error: `Invalid`, `TooLarge`, `UnsupportedFormat`, `Converter` |

## The item

| Field | Value |
| --- | --- |
| `title` | the converter's title (HTML `<title>`), else the first markdown heading (never for code), else the file name or origin |
| `body` | `DocumentBody::Text(markdown)` |
| `mime` | the detected format's canonical type (`text/markdown`, `text/html`, `text/x-source`, ...) |
| `meta` | the caller's, with `language` filled from the extension when unset |

## PDF and Office documents

Not handled by default. They need a real extractor, and which one a deployment
uses is its own decision — an in-process crate, a TinyBus module, a service. So
conversion is a trait a host binds:

```rust,ignore
let chain = ConverterChain::default().prepend(Box::new(MyPdfConverter));
```

The `documents-office` feature ships one such binding, `OfficeConverter`: PDF (text
layer only — a scanned PDF is refused as having no text; pages are extracted
one by one, normalized on their own and joined by `PAGE_BREAK`, a form feed,
so a reader can number them), DOCX, PPTX (slides
in numeric order) and XLSX (one `sheet | cell | cell` line per row), all pure
Rust. It refuses hostile input rather than allocating for it: an archive whose
declared uncompressed size exceeds `MAX_DECOMPRESSED_BYTES` (64 MiB), and a
spreadsheet whose dense used range exceeds `MAX_SPREADSHEET_DENSE_CELLS`
(1,000,000). Unreadable documents are `Error::Invalid`. Parsing is CPU-bound;
a host on a shared executor calls `OfficeConverter::convert_blocking` from its
own blocking pool.

A zip upload is told apart by its part names (`word/`, `xl/`, `ppt/`) read
from the central directory, so an `.xlsx` sent as `application/octet-stream`
still sniffs as XLSX. When the parts say nothing, an Office label refines the
container, and `Docx` is the fallback.

A format nothing in the chain claims is `Error::UnsupportedFormat`, naming the
format and listing what the build *can* convert. It is never a silent empty
document — storing an empty body loses the upload while looking like a
success.

## Operational constraints

- **Size is capped before conversion.** `MAX_DOCUMENT_BYTES` (32 MiB) is
  checked on the raw bytes, because a document that would not fit is one this
  process should never finish decoding.
- **A conversion that produces no text is an error**, not an empty item.
- **Errors map onto the contract.** `From<Error> for tinymemory_api::Error`:
  input problems are `InvalidRequest`, a missing converter is `Unsupported`,
  a converter's own failure is `Engine`.

## Features

- `documents` — this module; depends only on `async-trait`, `serde`,
  `serde_json` and `thiserror`.
- `documents-office` — `OfficeConverter` (`pdf-extract`, `calamine`, `zip`,
  `quick-xml`). Off by default; it links a PDF parser and a spreadsheet reader
  a text-only host has no use for.
