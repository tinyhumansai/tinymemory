//! The converter seam: bytes in, markdown out.
//!
//! Markdown is the intermediate form for everything the memory layer ingests,
//! because it is the one format that survives every hop the content makes —
//! chunkers split on its headings, embedders read it as prose, agents are
//! trained on it, and a human can read the stored copy without a renderer.
//!
//! ## Why this is a trait
//!
//! Text, markdown and HTML convert with no dependencies, and this module does
//! them ([`NativeConverter`]). PDF and the Office formats do not: they need a
//! real extractor, and which extractor a deployment uses is its own decision —
//! an in-process crate, a TinyBus module, a service. So conversion is a trait a
//! host binds rather than a fixed table, and [`ConverterChain`] composes the
//! native converter with whatever the host brings — including this module's own
//! `OfficeConverter` when the `documents-office` feature is on.
//!
//! Source code is textual too, and [`NativeConverter`] stores it exactly as
//! written: reflowing it or running it through the HTML converter would change
//! what the code says. Its language rides along in
//! [`ConvertedDocument::language`].
//!
//! A format with no converter is [`Error::UnsupportedFormat`] naming the
//! format, never a silent empty document.

mod types;

use async_trait::async_trait;

use crate::documents::error::{Error, Result};
use crate::documents::format::DocumentFormat;
use crate::documents::html;
use crate::documents::language::language_for_path;

pub use types::{ConvertedDocument, MAX_DOCUMENT_BYTES, RawDocument};

/// Turns a document of some format into markdown.
///
/// Object-safe and async: a converter that shells out to a bus module or an
/// HTTP service is as bindable as one that runs in-process.
#[async_trait]
pub trait DocumentConverter: Send + Sync {
    /// A short name for this converter, for diagnostics and metadata.
    fn name(&self) -> &str;

    /// Whether this converter handles `format`.
    ///
    /// Consulted before [`Self::convert`] so a chain can skip a converter
    /// without paying for a failed attempt.
    fn supports(&self, format: DocumentFormat) -> bool;

    /// Convert `document` to markdown.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFormat`] for a format this converter does not
    /// handle, [`Error::Invalid`] for a document it cannot decode,
    /// [`Error::TooLarge`] for one over [`MAX_DOCUMENT_BYTES`], and
    /// [`Error::Converter`] for the converter's own failure.
    async fn convert(&self, document: &RawDocument) -> Result<ConvertedDocument>;
}

/// Reject a document that is empty or over the size cap.
///
/// Every converter should call this first. Free-standing rather than a default
/// method so a converter that overrides nothing else still cannot forget it by
/// implementing `convert` from scratch — the check is one call, and a missing
/// call is visible in review.
///
/// # Errors
///
/// [`Error::Invalid`] for an empty body, [`Error::TooLarge`] for one over
/// [`MAX_DOCUMENT_BYTES`].
pub fn check_size(document: &RawDocument) -> Result<()> {
    if document.bytes.is_empty() {
        return Err(Error::Invalid("document body is empty".to_string()));
    }
    if document.bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::TooLarge {
            size: document.bytes.len(),
            limit: MAX_DOCUMENT_BYTES,
        });
    }
    Ok(())
}

/// Turn already-decoded text of a textual `format` into markdown.
///
/// The synchronous core of [`NativeConverter`], for callers that already hold
/// a `String` (a source reader's body) rather than a byte buffer: HTML goes
/// through [`html::to_markdown`], and markdown, plain text and code are
/// returned exactly as written. A non-textual format is returned unchanged
/// too, because there is nothing this function could decode it with.
#[must_use]
pub fn markdown_from_text(text: &str, format: DocumentFormat) -> String {
    match format {
        DocumentFormat::Html => html::to_markdown(text),
        _ => text.to_string(),
    }
}

/// The formats this module converts without help: markdown, plain text, HTML
/// and source code.
///
/// Everything it handles is already text, so the whole implementation is
/// decoding plus, for HTML, [`crate::documents::html::to_markdown`]. PDF and the Office
/// formats are deliberately absent — see the module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeConverter;

#[async_trait]
impl DocumentConverter for NativeConverter {
    fn name(&self) -> &str {
        "native"
    }

    fn supports(&self, format: DocumentFormat) -> bool {
        format.is_textual()
    }

    async fn convert(&self, document: &RawDocument) -> Result<ConvertedDocument> {
        check_size(document)?;
        let format = document.format();
        if !self.supports(format) {
            return Err(Error::UnsupportedFormat(format!(
                "the native converter does not handle {format}; bind a converter that does"
            )));
        }
        let text = std::str::from_utf8(&document.bytes)
            .map_err(|error| Error::Invalid(format!("document is not valid utf-8: {error}")))?;

        // Plain text and code are valid markdown as written. Rewriting them —
        // escaping, wrapping, guessing at headings — would change the user's
        // words or the program's meaning.
        let markdown = markdown_from_text(text, format);
        let title = match format {
            DocumentFormat::Html => html::extract_title(text),
            _ => None,
        };
        let language = match format {
            DocumentFormat::Code => document
                .filename
                .as_deref()
                .and_then(language_for_path)
                .map(str::to_string),
            _ => None,
        };

        if markdown.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "converting {format} produced no text"
            )));
        }

        Ok(
            ConvertedDocument::new(markdown, format, document.bytes.len())
                .with_title(title)
                .with_language(language)
                .with_metadata(serde_json::json!({ "converter": self.name() })),
        )
    }
}

/// Tries each converter in order and uses the first that claims the format.
///
/// Order is priority: a host that wants its own HTML handling puts it before
/// [`NativeConverter`]. The chain does not fall through on failure — a
/// converter that claims a format and then fails has found a real problem, and
/// retrying it against a converter that already declined would turn a precise
/// error into a vague one.
pub struct ConverterChain {
    converters: Vec<Box<dyn DocumentConverter>>,
}

impl std::fmt::Debug for ConverterChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConverterChain")
            .field(
                "converters",
                &self.converters.iter().map(|c| c.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for ConverterChain {
    /// A chain holding only [`NativeConverter`] — text, markdown and HTML, and
    /// a clear error for anything else.
    fn default() -> Self {
        Self::new(vec![Box::new(NativeConverter)])
    }
}

impl ConverterChain {
    /// Build a chain from converters in priority order.
    #[must_use]
    pub fn new(converters: Vec<Box<dyn DocumentConverter>>) -> Self {
        Self { converters }
    }

    /// Put `converter` ahead of everything already in the chain.
    #[must_use]
    pub fn prepend(mut self, converter: Box<dyn DocumentConverter>) -> Self {
        self.converters.insert(0, converter);
        self
    }

    /// Put `converter` behind everything already in the chain.
    #[must_use]
    pub fn push(mut self, converter: Box<dyn DocumentConverter>) -> Self {
        self.converters.push(converter);
        self
    }

    /// Every format some converter in this chain claims.
    #[must_use]
    pub fn supported_formats(&self) -> Vec<DocumentFormat> {
        [
            DocumentFormat::Markdown,
            DocumentFormat::PlainText,
            DocumentFormat::Html,
            DocumentFormat::Code,
            DocumentFormat::Pdf,
            DocumentFormat::Docx,
            DocumentFormat::Xlsx,
            DocumentFormat::Pptx,
        ]
        .into_iter()
        .filter(|format| self.supports(*format))
        .collect()
    }
}

#[async_trait]
impl DocumentConverter for ConverterChain {
    fn name(&self) -> &str {
        "chain"
    }

    fn supports(&self, format: DocumentFormat) -> bool {
        self.converters.iter().any(|c| c.supports(format))
    }

    async fn convert(&self, document: &RawDocument) -> Result<ConvertedDocument> {
        check_size(document)?;
        let format = document.format();
        match self.converters.iter().find(|c| c.supports(format)) {
            Some(converter) => converter.convert(document).await,
            None => Err(Error::UnsupportedFormat(format!(
                "no converter handles {format}; this build converts {}",
                describe(&self.supported_formats())
            ))),
        }
    }
}

/// Render a format list for an error message.
fn describe(formats: &[DocumentFormat]) -> String {
    if formats.is_empty() {
        return "nothing".to_string();
    }
    formats
        .iter()
        .map(DocumentFormat::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
