//! Fetching a URL into a [`RawDocument`].
//!
//! ## Why this reuses the source readers' guard
//!
//! A URL a user types is an SSRF vector: `http://169.254.169.254/` is a cloud
//! metadata endpoint, `http://localhost:6379/` is somebody's Redis, and a
//! hostname that resolves publicly on the first lookup can resolve to a private
//! address on the second. `tinymemory-sources` already solved this for the RSS
//! and web-page readers — a scheme and host policy plus a resolver that pins
//! connections to globally routable addresses — and this module uses that
//! guard rather than growing a second one. Two SSRF implementations in one
//! workspace means one of them is the weaker, and nobody knows which.
//!
//! ## What it does not do
//!
//! No scheduling, no retries, no credentials, no robots.txt. Those are host
//! policy, and the same rule that keeps them out of a driver keeps them out of
//! here: this fetches one URL, once, when asked.

use tinymemory_api::error::MemoryError;
use tinymemory_sources::readers::ssrf::{build_client, is_url_allowed, read_body_capped};

use crate::convert::{RawDocument, MAX_DOCUMENT_BYTES};
use crate::error::Result;

/// Fetch `url` and return its body as a [`RawDocument`].
///
/// The response's `Content-Type` becomes the document's declared MIME type and
/// the URL becomes its origin, so format detection and key derivation both have
/// what they need without the caller repeating itself.
///
/// # Errors
///
/// - [`MemoryError::Invalid`] for a malformed URL, or one the SSRF guard
///   refuses.
/// - [`MemoryError::Unreachable`] when the request never completed.
/// - [`MemoryError::Backend`] for a non-success status.
/// - [`MemoryError::BudgetExceeded`] for a body over
///   [`MAX_DOCUMENT_BYTES`].
pub async fn fetch_url(url: &str) -> Result<RawDocument> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| MemoryError::Invalid(format!("invalid url {url:?}: {error}")))?;
    if !is_url_allowed(&parsed) {
        return Err(MemoryError::Invalid(format!(
            "url {url:?} is not an allowed fetch target"
        )));
    }

    let client = build_client().map_err(MemoryError::Backend)?;
    let response = client
        .get(parsed.clone())
        .send()
        .await
        .map_err(|error| MemoryError::Unreachable(format!("fetching {url:?}: {error}")))?;

    response_to_document(url, parsed, response).await
}

/// Validate and convert a completed HTTP response.
async fn response_to_document(
    url: &str,
    parsed: reqwest::Url,
    response: reqwest::Response,
) -> Result<RawDocument> {
    let status = response.status();
    if !status.is_success() {
        return Err(MemoryError::Backend(format!(
            "fetching {url:?} answered {status}"
        )));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    // The cap is applied while reading, not after: a body that would not fit is
    // one this process should never have finished buffering.
    let bytes = read_body_capped(response, MAX_DOCUMENT_BYTES as u64)
        .await
        .map_err(|error| read_error(url, &error))?;

    if bytes.is_empty() {
        return Err(MemoryError::Invalid(format!("{url:?} returned no body")));
    }

    let mut document = RawDocument::new(bytes).with_origin(parsed.to_string());
    if let Some(content_type) = content_type {
        document = document.with_mime(content_type);
    }
    // A URL's last path segment is often the only filename there is, and format
    // detection falls back to it when the server sent no useful type.
    if let Some(name) = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| !name.is_empty() && name.contains('.'))
    {
        document = document.with_filename(name.to_string());
    }
    Ok(document)
}

/// Turn a `read_body_capped` failure into the right [`MemoryError`] variant.
///
/// `read_body_capped` collapses two different failures into one `String`: a
/// body over the size cap, and a stream that failed mid-read. Those need
/// different retry policies from a caller, so this tells them apart by the
/// message `read_body_capped` always uses for the size case, rather than
/// reporting every failure as a budget overrun.
fn read_error(url: &str, error: &str) -> MemoryError {
    if error.contains("exceeds") && error.contains("-byte limit") {
        MemoryError::BudgetExceeded(format!("reading {url:?}: {error}"))
    } else {
        MemoryError::Unreachable(format!("reading {url:?}: {error}"))
    }
}

#[cfg(test)]
mod test;
