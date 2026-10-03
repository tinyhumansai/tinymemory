//! A PDF's text layer.

use super::unreadable;
use crate::error::Result;

/// Extracts a PDF's text layer, which may be empty.
///
/// A scanned PDF has none: the file was read, it simply carries pictures of
/// words. That comes back as empty text, and the converter reports it as a
/// document with no text rather than as a parse failure.
pub(super) fn extract(bytes: &[u8]) -> Result<String> {
    // `pdf-extract` panics on some malformed documents rather than erroring.
    // Caught so one bad file is one refused document, not a crashed task.
    match std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(error)) => Err(unreadable(format!("the PDF could not be read: {error}"))),
        Err(_) => Err(unreadable(
            "the PDF is malformed enough that the parser gave up on it".to_string(),
        )),
    }
}
