//! PDF and Office Open XML conversion (the `documents-office` feature).
//!
//! [`crate::documents::convert::NativeConverter`] handles what is already text. This is
//! the converter for the formats people actually drop into memory that are not
//! — a contract PDF, a spec `.docx`, a pricing `.xlsx`, a deck — so a host
//! does not have to bind an extractor of its own for them:
//!
//! ```
//! use tinymemory_integrations::documents::{ConverterChain, OfficeConverter};
//!
//! let chain = ConverterChain::default().prepend(Box::new(OfficeConverter));
//! ```
//!
//! | Format | Reader | Markdown it produces |
//! | --- | --- | --- |
//! | PDF | `pdf-extract`, text layer only | each page's text, whitespace-normalized, pages joined by [`PAGE_BREAK`] |
//! | DOCX | `zip` + `quick-xml` over `word/document.xml` | one paragraph per `w:p` |
//! | PPTX | `zip` + `quick-xml` over `ppt/slides/slideN.xml` | slides in numeric order, one paragraph per `a:p` |
//! | XLSX | `calamine` | one `sheet \| cell \| cell` line per non-empty row |
//!
//! A spreadsheet is flattened rather than rebuilt as a table because that is
//! what recall can use: a chunk reading `Q3 | EMEA | 412000` answers a question
//! about EMEA revenue, and the same data as aligned columns does not survive
//! chunking.
//!
//! ## Hostile input
//!
//! [`crate::documents::convert::MAX_DOCUMENT_BYTES`] caps the *compressed* upload, but an
//! Office file is a zip, and a small highly compressed part can expand without
//! limit. Every archive is therefore refused when the uncompressed sizes its
//! central directory declares sum past [`MAX_DECOMPRESSED_BYTES`] — checked
//! before any entry is read — and each entry read is capped as well, so an
//! archive that lies about its sizes cannot force the allocation either.
//!
//! A spreadsheet has one more: calamine materializes the dense bounding box of
//! a sheet's cells, so a tiny workbook with one cell at `A1` and one at
//! `XFD1048576` would allocate ~17 billion cells. The cells are scanned
//! sparsely first and a sheet whose box exceeds
//! [`MAX_SPREADSHEET_DENSE_CELLS`] is refused before that happens.
//!
//! `pdf-extract` panics on some malformed documents rather than erroring; the
//! panic is caught and reported as an unreadable document.
//!
//! ## Blocking work
//!
//! Parsing is CPU-bound and synchronous. The async
//! [`DocumentConverter::convert`] runs it inline, which is right for a
//! current-thread runtime or a small document; a host on a shared executor
//! calls [`OfficeConverter::convert_blocking`] from its own blocking pool
//! instead, and gets the same result.

mod normalize;
mod ooxml;
mod pdf;
mod xlsx;

use async_trait::async_trait;

use crate::documents::PAGE_BREAK;
#[cfg(test)]
use crate::documents::convert::MAX_DOCUMENT_BYTES;
use crate::documents::convert::{ConvertedDocument, DocumentConverter, RawDocument, check_size};
use crate::documents::error::{Error, Result};
use crate::documents::format::DocumentFormat;

/// The largest uncompressed size an Office archive may declare, in bytes,
/// before it is refused as a likely zip bomb. See the module docs.
pub const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// The most cells a spreadsheet's dense used range may span before it is
/// refused. The used range of a real spreadsheet is a small fraction of the
/// grid, so this only bites hostile input; see the module docs.
pub const MAX_SPREADSHEET_DENSE_CELLS: usize = 1_000_000;

/// Converts PDF, DOCX, PPTX and XLSX documents to markdown, in-process.
///
/// Claims exactly those four formats, so it composes with
/// [`crate::documents::convert::NativeConverter`] in a [`crate::documents::convert::ConverterChain`]
/// without shadowing it. A document whose text cannot be read — malformed,
/// over a cap, or a scanned PDF with no text layer — is [`Error::Invalid`]
/// saying which, never an empty document.
#[derive(Debug, Default, Clone, Copy)]
pub struct OfficeConverter;

impl OfficeConverter {
    /// The synchronous conversion behind [`DocumentConverter::convert`], for a
    /// host that runs CPU-bound parsing on its own blocking pool.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFormat`] for a format this converter does not
    /// claim; [`Error::Invalid`] for an empty body, a document that cannot be
    /// read or exceeds a decoding cap, or one with no extractable text;
    /// [`Error::TooLarge`] for a body over
    /// [`crate::documents::convert::MAX_DOCUMENT_BYTES`].
    pub fn convert_blocking(&self, document: &RawDocument) -> Result<ConvertedDocument> {
        check_size(document)?;
        let format = document.format();
        let bytes = document.bytes.as_slice();
        let markdown = match format {
            DocumentFormat::Pdf => paged(&pdf::extract(bytes)?),
            DocumentFormat::Docx => normalize::normalize(&ooxml::docx(bytes)?),
            DocumentFormat::Pptx => normalize::normalize(&ooxml::pptx(bytes)?),
            DocumentFormat::Xlsx => normalize::normalize(&xlsx::extract(bytes)?),
            other => {
                return Err(Error::UnsupportedFormat(format!(
                    "the office converter does not handle {other}"
                )));
            }
        };
        if markdown.trim().is_empty() {
            return Err(unreadable(format!("converting {format} produced no text")));
        }
        Ok(ConvertedDocument::new(markdown, format, bytes.len())
            .with_metadata(serde_json::json!({ "converter": self.name() })))
    }
}

/// A PDF's pages as one markdown text: each page normalized on its own,
/// joined by [`PAGE_BREAK`] so a reader can still tell where each page
/// starts (an empty page keeps its place, so numbering survives).
fn paged(pages: &[String]) -> String {
    pages
        .iter()
        .map(|page| normalize::normalize(page))
        .collect::<Vec<_>>()
        .join(&PAGE_BREAK.to_string())
}

#[async_trait]
impl DocumentConverter for OfficeConverter {
    fn name(&self) -> &str {
        "office"
    }

    fn supports(&self, format: DocumentFormat) -> bool {
        matches!(
            format,
            DocumentFormat::Pdf
                | DocumentFormat::Docx
                | DocumentFormat::Xlsx
                | DocumentFormat::Pptx
        )
    }

    async fn convert(&self, document: &RawDocument) -> Result<ConvertedDocument> {
        self.convert_blocking(document)
    }
}

/// The error for a document this converter could not turn into text.
///
/// One constructor for every refusal in this module, so the readers say *why*
/// in their own words and agree on the variant. [`Error::Invalid`] rather than
/// [`Error::Converter`]: a malformed or hostile document is a problem with the
/// input, not a fault in the converter.
fn unreadable(reason: String) -> Error {
    Error::Invalid(reason)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
