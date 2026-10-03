//! Document intake for TinyMemory: format sniffing, conversion to markdown,
//! and the [`StoreItem::Document`](tinymemory_api::StoreItem::Document) an
//! engine stores.
//!
//! Getting a PDF, a `.docx`, an HTML export, a source file or a note into
//! memory is three steps:
//!
//! 1. **Work out what it is.** [`DocumentFormat::sniff`] reads magic bytes,
//!    the declared MIME type and the filename, in that order. Source code is
//!    recognised by its name ([`language_for_path`]).
//! 2. **Turn it into markdown.** [`DocumentConverter`] is the seam;
//!    [`NativeConverter`] covers markdown, plain text, HTML and code with no
//!    dependencies; a host binds its own for PDF and Office documents, or
//!    prepends `OfficeConverter` (feature `office`) for PDF, DOCX, PPTX and
//!    XLSX.
//! 3. **Wrap it as an item.** [`document_item`] produces a
//!    `StoreItem::Document` with the caller's
//!    [`MemoryMeta`](tinymemory_api::MemoryMeta), filling `language` from the
//!    file extension when the caller left it unset.
//!
//! This crate does no I/O. Reading files and fetching URLs belongs to
//! `tinymemory-sources`, which depends on this crate for conversion.
//!
//! # Example
//!
//! ```
//! use tinymemory_api::{DocumentBody, MemoryMeta, SourceKind, StoreItem};
//! use tinymemory_documents::{ConverterChain, RawDocument, document_item};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let chain = ConverterChain::default();
//! let file = RawDocument::new("fn main() {}\n").with_filename("src/main.rs");
//! let meta = MemoryMeta::from_source(SourceKind::File, None);
//!
//! let item = document_item(&chain, &file, meta).await?;
//! let StoreItem::Document { title, body, meta, .. } = item else {
//!     unreachable!("document_item always builds a document");
//! };
//! assert_eq!(title.as_deref(), Some("main.rs"));
//! assert_eq!(body, DocumentBody::Text("fn main() {}\n".into()));
//! assert_eq!(meta.language.as_deref(), Some("rust"));
//! # Ok::<(), tinymemory_documents::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod convert;
pub mod error;
pub mod format;
pub mod html;
pub mod item;
pub mod language;
#[cfg(feature = "office")]
pub mod office;

pub use convert::{
    ConvertedDocument, ConverterChain, DocumentConverter, MAX_DOCUMENT_BYTES, NativeConverter,
    RawDocument, check_size, markdown_from_text,
};
pub use error::{Error, Result};
pub use format::DocumentFormat;
pub use item::{converted_item, document_item};
pub use language::language_for_path;
#[cfg(feature = "office")]
pub use office::OfficeConverter;
