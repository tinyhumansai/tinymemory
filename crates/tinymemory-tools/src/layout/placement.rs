//! Where a brain document is filed: the placement rules every host shares.
//!
//! A document goes to the `source:<id>` node of the connector it came from
//! ([`MemoryLayout::brain`]), or, for GitHub with the per-repository split on,
//! to its repository's collection below it
//! ([`MemoryLayout::brain_collection`]). Local files of every format go to
//! [`BrainSource::Files`] (`source:files`), the unit a host erases.
//!
//! Connector vocabulary (which of a host's source kinds reads as which
//! [`BrainSource`]) stays with the host; these rules start from the
//! [`BrainSource`] it chose.

use tinymemory_api::{Namespace, Result, SourceKind, StoreItem};

use super::{BrainSource, MemoryLayout};

/// The brain source of local files and uploads, of any format
/// ([`BrainSource::Files`]; its node is `source:files`).
#[must_use]
pub fn files_source() -> BrainSource {
    BrainSource::Files
}

/// Canonical form of a connector slug found on a legacy brain node
/// (`google_drive` becomes `googledrive`), so documents filed by the removed
/// Composio sync still migrate to the node they were filed under.
fn legacy_connector_slug(slug: &str) -> String {
    let key = slug.trim().to_ascii_lowercase();
    match key.as_str() {
        "feishu" | "lark" => "larksuite".to_string(),
        "google_calendar" => "googlecalendar".to_string(),
        "google_drive" => "googledrive".to_string(),
        "google_sheets" => "googlesheets".to_string(),
        _ => key,
    }
}

/// The brain source a document filed under the old per-type layout belongs
/// to now: `old_source_id` is the id of the `source:<id>` node it sits at,
/// `item` the document itself.
///
/// - Per-format nodes (`pdf`, `markdown`, `docx`, `xlsx`, `pptx`, `code`,
///   `other`) held local files: `files`.
/// - `web` held links and feeds, but also HTML files from a folder or an
///   upload; a document with a file kind or a file path is a file.
/// - Any other node was a connector: its canonical slug (`google_drive`
///   becomes `googledrive`), so `notion`, `github` and `gmail` stay put.
#[must_use]
pub fn legacy_brain_node(old_source_id: &str, item: &StoreItem) -> BrainSource {
    let id = legacy_connector_slug(old_source_id);
    match id.as_str() {
        "files" | "pdf" | "markdown" | "md" | "docx" | "xlsx" | "pptx" | "code" | "other" => {
            files_source()
        }
        "web" => {
            let meta = item.meta();
            let file = matches!(meta.source.kind, SourceKind::File | SourceKind::Folder)
                || meta.file_path.is_some();
            if file {
                files_source()
            } else {
                BrainSource::Web
            }
        }
        _ => id.parse().unwrap_or_else(|_| files_source()),
    }
}

/// The repository a GitHub document belongs to, as the collection id
/// `<owner>--<repo>` (lowercase, as GitHub's names are case-insensitive):
/// from its `repo` (`owner/name` or a URL), else its URL. An owner name
/// never holds `--` nor ends in `-`, so the first `--` always splits the
/// two and no two repositories share an id (`foo-bar/repo` and
/// `foo/bar-repo` stay apart).
#[must_use]
pub fn github_collection(item: &StoreItem) -> Option<String> {
    let meta = item.meta();
    [meta.repo.as_deref(), meta.url.as_deref()]
        .into_iter()
        .flatten()
        .find_map(|raw| {
            let path = raw
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("www.")
                .trim_start_matches("github.com/");
            // A URL's query or fragment is not part of the path.
            let path = path.split(['?', '#']).next().unwrap_or_default();
            let mut parts = path.split('/').filter(|part| !part.is_empty());
            let (owner, repo) = (parts.next()?, parts.next()?);
            let repo = repo.trim_end_matches(".git");
            (!owner.contains('.') && !repo.is_empty())
                .then(|| format!("{owner}--{repo}").to_ascii_lowercase())
        })
}

/// The node `item` of `source` is filed at: the source's node, or, for
/// GitHub with `split_github_by_repo` on, its repository's collection below
/// it.
///
/// # Errors
///
/// When the node cannot be built (an id the namespace grammar refuses).
pub fn brain_node_with(
    split_github_by_repo: bool,
    layout: &MemoryLayout,
    source: &BrainSource,
    item: &StoreItem,
) -> Result<Namespace> {
    if split_github_by_repo
        && *source == BrainSource::Github
        && let Some(repo) = github_collection(item)
    {
        return layout.brain_collection(source, &repo);
    }
    layout.brain(source)
}

/// `item` placed in the brain at `node`, with no agent id (the brain
/// belongs to every agent).
#[must_use]
pub fn file_into(node: Namespace, mut item: StoreItem) -> StoreItem {
    let meta = item.meta_mut();
    meta.namespace = node;
    meta.agent_id = None;
    item
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod tests;
