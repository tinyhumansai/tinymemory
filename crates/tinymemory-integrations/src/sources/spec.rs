//! Host source rules that do not depend on its configuration store.

use tinymemory_api::Namespace;
use tinymemory_tools::MemoryLayout;

use super::error::{Error, Result};
use super::sync::MIN_SCHEDULE_MINS;
use super::types::{MemorySourceConfig, MemorySourceKind};

/// The fields that determine whether a source can be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// The reader kind.
    pub kind: MemorySourceKind,
    /// A path, URL, or GitHub `owner/repo` target.
    pub target: String,
    /// Minutes between scheduled reads; unset means manual only.
    pub schedule_mins: Option<u32>,
    /// Optional namespace root for the source.
    pub namespace: Option<String>,
}

impl SourceSpec {
    /// Trims and normalizes a target for `kind`.
    ///
    /// Folder and file targets remain paths. GitHub targets accept an
    /// `owner/repo` shorthand or an HTTP(S) URL. Link and RSS targets must be
    /// HTTP(S) URLs.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the target is blank, malformed, or uses a
    /// scheme other than HTTP(S) for a network source.
    pub fn normalize_target(kind: MemorySourceKind, target: &str) -> Result<String> {
        let target = target.trim();
        if target.is_empty() {
            return Err(Error::Invalid("target must not be empty".to_string()));
        }
        match kind {
            MemorySourceKind::Folder | MemorySourceKind::File => Ok(target.to_string()),
            MemorySourceKind::Github => {
                if target.starts_with("http://") || target.starts_with("https://") {
                    http_url(target)
                } else {
                    let mut parts = target.split('/');
                    match (parts.next(), parts.next(), parts.next()) {
                        (Some(owner), Some(repo), None)
                            if !owner.is_empty() && !repo.is_empty() =>
                        {
                            Ok(format!("https://github.com/{owner}/{repo}"))
                        }
                        _ => Err(Error::Invalid(
                            "a GitHub target is `owner/repo` or a repository URL".to_string(),
                        )),
                    }
                }
            }
            MemorySourceKind::Link | MemorySourceKind::Rss => http_url(target),
        }
    }

    /// Checks the target, schedule, namespace, and duplicates before adding a
    /// source. Returns its normalized target.
    ///
    /// Duplicate identity matches the host wire contract: `(kind, target)`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when any field is invalid, the schedule is below
    /// [`MIN_SCHEDULE_MINS`], or a source with the same kind and normalized
    /// target already exists.
    pub fn validate_new(&self, existing: &[MemorySourceConfig]) -> Result<String> {
        let target = Self::normalize_target(self.kind, &self.target)?;
        if self
            .schedule_mins
            .is_some_and(|minutes| minutes < MIN_SCHEDULE_MINS)
        {
            return Err(Error::Invalid(format!(
                "schedule_mins must be at least {MIN_SCHEDULE_MINS}"
            )));
        }
        if existing
            .iter()
            .any(|source| source.kind == self.kind && source.target == target)
        {
            return Err(Error::Invalid("that source is already added".to_string()));
        }
        if let Some(namespace) = self
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let root = namespace
                .parse::<Namespace>()
                .map_err(|error| Error::Invalid(error.to_string()))?;
            MemoryLayout::new(root).map_err(|error| Error::Invalid(error.to_string()))?;
        }
        Ok(target)
    }

    /// Parses this source's namespace root, when set and non-blank.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the namespace is malformed.
    pub fn namespace(&self) -> Result<Option<Namespace>> {
        self.namespace
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<Namespace>()
                    .map_err(|error| Error::Invalid(error.to_string()))
            })
            .transpose()
    }
}

/// The layout this source files into, falling back to `configured` when its
/// namespace is missing or invalid. Invalid saved values are logged and do
/// not prevent the source from syncing.
#[must_use]
pub fn layout_of_source(configured: &MemoryLayout, source: &MemorySourceConfig) -> MemoryLayout {
    let Some(namespace) = source
        .namespace
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
    else {
        return configured.clone();
    };
    namespace
        .parse::<Namespace>()
        .map_err(|error| error.to_string())
        .and_then(|root| MemoryLayout::new(root).map_err(|error| error.to_string()))
        .unwrap_or_else(|error| {
            tracing::warn!(id = %source.id, %error, "[memory:sources] invalid root; using the configured one");
            configured.clone()
        })
}

/// The source's layout, or `configured` for an unknown source id.
#[must_use]
pub fn layout_of(
    configured: &MemoryLayout,
    sources: &[MemorySourceConfig],
    source_id: &str,
) -> MemoryLayout {
    sources
        .iter()
        .find(|source| source.id == source_id)
        .map_or_else(
            || configured.clone(),
            |source| layout_of_source(configured, source),
        )
}

fn http_url(target: &str) -> Result<String> {
    let url =
        url::Url::parse(target).map_err(|_| Error::Invalid("target is not a URL".to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Invalid("target must be an http(s) URL".to_string()));
    }
    Ok(url.to_string())
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
